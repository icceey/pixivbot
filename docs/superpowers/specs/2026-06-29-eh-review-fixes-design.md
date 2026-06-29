# E-Hentai Review Fixes Design

## Context

The review of `origin/master..HEAD` found correctness issues in the new E-Hentai/ExHentai subscription and decoupled download pipeline. The default behavior that EH is enabled when `[ehentai]` is omitted is intentional; this design keeps that default and adds an explicit opt-out flag.

## Goals

1. Prevent subscription collection from losing galleries when `max_push_per_tick` truncates a batch.
2. Make publish retries resume without repeating already-confirmed Telegram surfaces.
3. Prevent partial direct-image ZIP downloads from being treated as successful gallery downloads.
4. Keep EH download queue entries idempotent under duplicate or concurrent enqueue requests.
5. Keep rate-limit accounting aligned with bytes already downloaded, not only fully published entries.
6. Reject or downgrade Telegraph work that cannot be consumed because no Telegraph token is configured.
7. Ensure permanent Telegraph upload failure can fall back to archive delivery when archive delivery is enabled.
8. Treat temporarily non-notifiable chats as deferred work, not retry-budget-consuming failures.
9. Fix EH-facing MarkdownV2 and command behavior so users get deterministic responses instead of silent drops.
10. Add `ehentai.enabled` as an explicit disable switch while preserving the default enabled behavior.

## Non-Goals

- Do not redesign the four-stage collect/download/upload/publish architecture.
- Do not implement gid-only `/edl` lookup; the command will be documented as URL-only.
- Do not add a new external job runner or distributed lock system.
- Do not guarantee absolute exactly-once Telegram delivery across a database write failure immediately after a successful Telegram send. The design persists per-surface progress immediately after each confirmed send, which prevents the reviewed multi-surface replay cases and mark-done replay cases once those markers are stored.

## Approach Considered

### Recommended: Minimal State Extension

Extend the existing subscription state and queue schema with the smallest durable fields needed for correctness:

- `EhTagState` gets a pending backlog plus a high-water timestamp for overflow batches.
- `eh_download_queue` gets per-surface publish markers.
- Queue enqueue logic becomes merge-on-existing and conflict-safe.
- Telegraph entry points validate token availability before enqueueing Telegraph-only work.

This keeps the current stage boundaries and focuses changes on the failure modes found in review.

### Rejected: Publish-Only Reordering

Moving `mark_done` before sends or sending only one surface would reduce some duplication paths but would create missed-delivery risks or remove required output surfaces. It does not solve the underlying multi-side-effect recovery problem.

### Rejected: Full Publish Sub-Pipeline

Splitting archive send, link send, and cleanup into separate queue stages would be cleaner long-term, but it is larger than necessary for the current review fixes and would add migration/test surface beyond the accepted scope.

## Data Model Changes

### EH Feature Toggle

Add `EhentaiConfig.enabled: bool` with default `true`.

- `EhentaiConfig::default()` keeps EH enabled by default.
- `EhentaiConfig::is_enabled()` returns `enabled && site in {"e-hentai", "exhentai"}`.
- `config.toml.example` documents `enabled = true` and `enabled = false` as the explicit way to disable EH.
- `src/main.rs` and bot command exposure continue to derive runtime EH availability from whether the EH client was initialized.

### Subscription Pending Backlog

Extend `EhTagState` with durable overflow state:

```rust
pub struct EhPendingGallery {
    pub gid: u64,
    pub token: String,
    pub title: String,
    pub posted: i64,
}

pub struct EhTagState {
    pub pushed_gids: Vec<u64>,
    pub latest_posted_ts: i64,
    pub pending_galleries: Vec<EhPendingGallery>,
    pub pending_high_water_ts: i64,
}
```

`pending_galleries` contains galleries that already matched the subscription's filters but could not be enqueued because the per-tick cap was reached. `pending_high_water_ts` records the maximum `posted` timestamp covered by the overflow batch. While pending entries exist, `latest_posted_ts` must not advance past them.

### Publish Progress Markers

Add nullable timestamp columns to `eh_download_queue`:

| Column | Purpose |
| --- | --- |
| `archive_sent_at` | Set immediately after Telegram document send succeeds and the marker update succeeds. |
| `telegraph_sent_at` | Set immediately after Telegraph-link message send succeeds and the marker update succeeds. |

These fields are preserved by stale-reset logic. They are cleared when an entry is reset from terminal state for a new request. Publish cleanup deletes ZIP files only after all required surfaces are confirmed and the entry is marked `done`.

### Duplicate Queue Migration

The unique `(chat_id, gid)` migration must keep the most useful existing row before adding the index. The row selection priority is:

1. Active progress: `publishing`, `uploaded`, `uploading`, `downloaded`, `downloading`, `pending`.
2. Completed row: `done`.
3. Failed row: `failed`.
4. Tie-break by newest `COALESCE(completed_at, started_at, created_at)`, then highest `id`.

This avoids deleting a newer row that contains `zip_path`, `telegraph_url`, or in-flight progress.

## Data Flow Changes

### Collect Stage

For each subscription:

1. Load `EhTagState`.
2. Enqueue from `pending_galleries` first, respecting `max_push_per_tick`.
3. Remove successfully enqueued pending entries and add their gids to `pushed_gids`.
4. If pending remains, save state without advancing `latest_posted_ts`.
5. If pending is empty, process newly fetched metadata, skip already pushed gids, apply subscription filters, enqueue up to remaining cap, and store overflow in `pending_galleries`.
6. If overflow exists, set `pending_high_water_ts` to the max `posted` in the eligible batch and do not advance `latest_posted_ts`.
7. If no overflow remains, advance `latest_posted_ts` to the max of the current cursor, newly enqueued entries, and `pending_high_water_ts`; then clear `pending_high_water_ts`.

The task-level search can continue using the minimum subscription cursor. Because individual subscriptions do not advance past unconsumed overflow, later ticks will not filter those galleries away.

### Download Stage

Archive downloads keep the existing behavior. Direct image fallback changes from partial success to all-or-error:

- If N image pages are discovered, all N images must be fetched and written.
- Any page fetch, image URL parse, image fetch, or ZIP write failure returns an error.
- An error leaves the queue entry retryable and does not mark it `downloaded`.
- A ZIP with fewer images than discovered is never published as success.

Rate limiting uses `completed_at` and `file_size` for all statuses at or beyond a completed download: `downloaded`, `uploading`, `uploaded`, `publishing`, and `done`.

### Upload Stage

Telegraph work is only allowed when a `TelegraphClient` exists.

- `/telegraph` rejects requests with a friendly message when no token is configured.
- `/edl ... telegraph=on` rejects the Telegraph option when no token is configured.
- `/esub ... telegraph=on` rejects the subscription option when no token is configured.
- Global `upload_telegraph=true` without a token is treated as disabled for enqueue decisions and logs a warning at startup.

If Telegraph upload permanently fails:

- If `send_archive=true` and a ZIP exists, clear the Telegraph requirement for that entry, set it back to publishable archive-only state, and preserve the failure reason for logs.
- If archive delivery is disabled or the ZIP is missing, mark the entry failed and notify the chat.

Telegraph page splitting reserves bytes for continuation links before calling `createPage`, so chunks near the API size limit do not fail because of the added “Next Page” node.

### Publish Stage

Publish determines required surfaces per entry:

- Archive is required when `send_archive=true` and `zip_path` exists.
- Telegraph link is required when `telegraph_url` exists.
- If neither surface is required, the entry fails with a clear internal error instead of being marked `done` silently.

For each required surface:

1. Skip it if the corresponding sent marker is already set.
2. Send the Telegram message.
3. Immediately persist the corresponding sent marker.
4. Continue with the next surface only after the marker is stored.

When all required surfaces are marked sent, mark the entry `done` and delete the ZIP. If `mark_done` fails after both sent markers were stored, the retry only marks done and cleans up; it does not resend messages.

### Temporarily Non-Notifiable Chats

`get_chat_if_should_notify()` returning no chat is treated as a defer condition:

- The worker releases the claimed entry back to its previous ready status.
- `retry_count` is not incremented.
- `next_retry_at` is set to a short defer interval so workers do not spin.
- Permanent send errors from Telegram still use normal retry/fail behavior.

## Queue Enqueue Semantics

`enqueue_eh_download()` remains keyed by `(chat_id, gid)` but becomes merge-based:

- Existing `done` or `failed` rows are reset to `pending` and all transient fields are cleared.
- Existing non-terminal rows are updated in place instead of returned unchanged.
- `telegraph` is merged with logical OR.
- `source` is upgraded to `direct` if either the old or new request is direct.
- `token` and `title` are refreshed from the newest request.
- `retry_count`, status, and existing progress are preserved for non-terminal rows unless the existing status cannot satisfy a newly merged Telegraph requirement; in that case the row moves to the earliest safe status that can still complete the merged requirement.
- If insert races with another request, the unique-conflict path reselects the row and applies the same merge.

## Bot and User-Facing Behavior

- EH MarkdownV2 output uses `teloxide::utils::markdown::escape` or existing escaping helpers for dynamic values and fixed labels containing MarkdownV2 special characters such as `E-Hentai`.
- `/list` renders EH task values through proper MarkdownV2 escaping, or parses `EhTaskKey` and escapes each displayed segment.
- `/download` no longer silently drops targets. The minimal behavior is to reject ambiguous mixed input: if an input contains an EH link plus Pixiv/Booru targets, or more than one EH link, reply with a clear message asking the user to use `/edl <url>` for one EH gallery at a time. Non-EH multi-target behavior remains unchanged.
- `/edl` help text is changed from `<url|gid>` to `<url>` because gid-only lookup is not implemented.
- Enqueue failures in `/download` EH handling are reported as failures, not followed by a success message.

## Testing Strategy

### Repo and Migration Tests

- `enqueue_eh_download` merges `telegraph`, `source`, `token`, and `title` for non-terminal rows.
- Concurrent duplicate enqueue resolves through conflict/reselect/merge and returns one logical queue row.
- Rate-limit accounting includes `downloaded`, `uploading`, `uploaded`, `publishing`, and `done` entries with `completed_at` and `file_size`.
- Publish sent markers survive stale reset and are cleared on terminal-row reset.
- Duplicate cleanup migration keeps the highest-progress row by the documented priority order.

### Scheduler Tests

- With `max_push_per_tick=3` and at least four matching galleries, the fourth gallery remains pending and is enqueued on a later tick instead of being filtered out by the cursor.
- Publish sends archive, persists `archive_sent_at`, then if link send fails the retry sends only the link.
- If both publish markers exist but `done` is not set, the retry marks done and does not send either surface.
- `send_archive=false` with no `telegraph_url` fails the entry rather than silently marking done.
- Chat-not-notifiable defer does not increment `retry_count` in download, upload, or publish.
- Permanent Telegraph upload failure falls back to archive-only publish when `send_archive=true` and ZIP exists.

### EH Client Tests

- Direct image fallback fails if any image page request fails.
- Direct image fallback fails if any image URL parse or image fetch fails.
- Direct image fallback succeeds only when all discovered images are written.
- Telegraph page splitting accounts for continuation-link size.

### Bot and Config Tests

- `ehentai.enabled=false` makes `is_enabled()` false and keeps EH commands unavailable through normal bot command exposure.
- Missing Telegraph token rejects `/telegraph`, `/edl ... telegraph=on`, and `/esub ... telegraph=on`.
- EH success/list messages are MarkdownV2-safe for task values containing `|`, `=`, `_`, `-`, and other special characters.
- `/download` with mixed EH and non-EH targets returns an explicit ambiguity message.
- `/download` with multiple EH links returns an explicit one-gallery-at-a-time message.
- `/edl` command description matches URL-only parsing.

## Verification

Focused checks:

```powershell
cargo test -p pixivbot eh_ --no-default-features
cargo test -p eh_client
```

Final repository check:

```powershell
make ci
```

Docs-only changes can be checked with:

```powershell
git diff --check -- docs/superpowers/specs/2026-06-29-eh-review-fixes-design.md
```

## Acceptance Criteria

- No EH subscription gallery is skipped solely because a previous tick hit `max_push_per_tick`.
- Publish retry does not resend surfaces whose sent markers were already persisted.
- Direct image fallback never publishes a ZIP missing discovered pages.
- Duplicate queue requests merge stronger semantics instead of silently ignoring them.
- Download quota blocks based on already consumed download bytes.
- Telegraph tasks cannot enter an unconsumable state when no token is configured.
- Archive fallback is attempted after permanent Telegraph upload failure when configured.
- Temporarily non-notifiable chats do not exhaust retry budget.
- EH user-visible MarkdownV2 messages are sendable.
- `ehentai.enabled=false` is a working explicit disable switch while default EH behavior remains enabled.
