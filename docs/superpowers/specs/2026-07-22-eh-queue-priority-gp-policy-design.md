# EH queue priority and archive policy fixes

## Context

The main EH download worker currently claims every eligible pending entry in
`created_at ASC` order. A large old backlog can therefore delay newly queued
work indefinitely. The archive GP gate also treats a per-archive cost above
`max_archive_gp_cost` as a temporary defer even though waiting cannot change
that configured policy. Finally, direct archive requests silently map donor
resolutions (`1600x` and `2400x`) to the generic `dltype=res` form, which does
not request the configured width and exposes only the default resample cost and
size.

## Queue ordering

Only the main download claim in `Repo::get_next_for_download()` changes. Its
existing eligibility filters remain unchanged: the entry must be pending, must
not belong to the background downloader, and must have no future retry delay.

Eligible entries are ordered in two groups using a fixed two-hour cutoff:

1. Entries created after the cutoff are processed first in `created_at ASC`
   order, preserving FIFO behavior for the recent queue.
2. Entries created at or before the cutoff are processed after all recent
   entries in `created_at DESC` order, so the newest old entry is retried first.

The entry id provides a deterministic tie-breaker in the same direction as its
group. The classification is derived at claim time and is not persisted or
shown as a new queue status. Background download ordering is unchanged because
background entries have already entered a download process.

## Per-archive GP rejection

The shared archive gate gains three outcomes: proceed, defer, and reject.
Only a numeric `DownloadCost::Gp(gp)` where `gp` is greater than
`max_archive_gp_cost` is rejected permanently. This includes every positive GP
cost when the configured maximum is zero.

Both the main and background download workers handle rejection by atomically
marking the queue entry `failed`, recording a short policy error, setting its
completion timestamp, clearing applicable retry/background claim state, and
skipping both the GP ledger reservation and archive POST. Policy rejection does
not consume a retry attempt.

The following existing temporary conditions continue to defer: byte-rate
exhaustion, rolling GP-budget exhaustion, and non-numeric cost states
(`Insufficient`, `Unavailable`, and `Unknown`). Free and unlocked archives
continue normally.

## Unsupported donor resolutions

The direct archive workflow supports only `780x`, `980x`, `1280x`, and
`original`. Configuration deserialization rejects `1600x`, `2400x`, and any
other unknown value for both `subscription_resolution` and
`download_resolution`, with an error listing the supported values.

`EhClient::prepare_archive_download()` independently validates the resolution
before performing network requests. This prevents callers outside the root
configuration path from silently falling back to the generic resample form.
The public example configuration and API documentation are updated to remove
donor resolutions. Implementing the separate H@H Downloader workflow is out of
scope.

## Error handling and concurrency

The queue claim keeps its existing conditional update, so competing workers
cannot claim the same row. The ordering changes only candidate selection.
Permanent GP rejection methods use current-state guards: the main path requires
the downloading state, while the background path requires a pending entry with
a running background claim. A stale worker therefore cannot overwrite a row
that was canceled or re-enqueued concurrently.

Resolution validation fails before GET or POST. GP rejection happens after the
safe Archiver-page GET has revealed the cost but before any ledger write or
GP-spending POST.

## Testing

Repository tests cover the complete ordering sequence: recent entries are
claimed first and FIFO, followed by entries older than two hours in reverse
creation order. Boundary coverage defines an entry exactly two hours old as
old, verifies future retries and background entries remain ineligible, and
retains the atomic-claim regression checks.

Scheduler tests cover numeric per-archive GP rejection in both main and
background workers, including failed status, recorded reason, no POST, no
ledger reservation, and no retry-count consumption. Existing tests continue to
prove that rolling budgets, byte limits, and unknown cost states defer.

Configuration and client tests accept all four supported resolutions and reject
donor or unknown values before network activity. Focused crate tests are
followed by formatting, Clippy with warnings denied, workspace checks/tests,
the available release build, and final Oracle review.
