# EH archive concurrent download design

## Context

EH archive downloads currently use one contiguous temporary file. When
`{archive}.zip.part` exists, `eh_client` sends `Range: bytes={existing_len}-`,
validates that the `206 Content-Range` starts at that length and reaches the
declared end of the object, then appends the response. This supports a single
resumable prefix but cannot use multiple HTTP connections for one archive or
represent disjoint completed ranges.

The application already supports downloading multiple queue entries in
parallel through `background_download_concurrency`. This design adds a separate
per-archive concurrency limit. It does not change queue-level concurrency.

## Goals

- Allow one EH archive to use multiple bounded HTTP Range requests concurrently.
- Configure the maximum connections per archive independently of queue-level
  concurrency.
- Start with one connection and create parts dynamically from observed download
  rates rather than pre-splitting the archive into fixed chunks.
- Reuse newly available connection slots by heuristically splitting the largest
  eligible remaining interval.
- Persist the dynamically created interval layout and downloaded bytes under the
  EH cache directory so an interrupted download can resume.
- Preserve current worker retry, `DownloadInProgress`, ZIP validation, GP/size
  gate, and final atomic rename behavior.
- Clean all multipart artifacts on success, permanent failure, or orphan cache
  cleanup.

## Non-goals

- Do not change how many archive queue entries can run concurrently.
- Do not add Telegram commands, status fields, or progress UI.
- Do not change the unauthenticated per-image fallback that builds a ZIP locally.
- Do not change archive selection, resolution, GP charging, size gates, upload,
  publishing, or subscription behavior.
- Do not add multipart state to the database; cache artifacts remain the source
  of resumable byte progress.
- Do not guarantee detection of a remote object replacement when the URL and
  total length stay unchanged and the server supplies no validator.

## Decisions

### Configuration

Add `ehentai.archive_download_concurrency` as the maximum number of active Range
requests for one archive.

- Default: `1`.
- `1`: new downloads retain the existing sequential `.zip.part` path.
- Greater than `1`: new downloads may use the multipart coordinator.
- `0`: invalid configuration and rejected during deserialization.
- A previously created multipart manifest is resumed even if the configured
  value is later reduced to `1`; the coordinator then runs with one active part.

The existing public `EhClient::download_archive()` and
`EhClient::download_archive_with_request()` methods remain compatible and use a
maximum concurrency of one. Add an options-based method used by both production
EH workers. The option type exposes only `max_concurrency`; rate and split
constants remain internal policy.

### Approaches considered

1. **Independent part files plus a versioned manifest (selected).** Each interval
   owns one append-only file. File length is durable progress, structural changes
   are recorded atomically, and completed files are concatenated in order.
2. **Concurrent random writes into one sparse temporary file.** This avoids a
   final merge but makes Windows file sharing, crash consistency, and completed
   range tracking more difficult.
3. **A pre-created pool of fixed chunks.** This is simpler to schedule but does
   not satisfy dynamic rate-based part creation and can create unnecessary Range
   requests and cache files.

## Architecture

### Module boundaries

Create a focused archive transfer module in `eh_client`. It owns:

- multipart artifact paths;
- the manifest and interval invariants;
- bounded Range response validation;
- per-part append and retry behavior;
- rate sampling and split decisions;
- task coordination, final assembly, and multipart cleanup.

`client.rs` continues to own the archiver POST and redirect parsing. After it
obtains the archive URL, it selects the sequential or multipart transfer path,
validates the assembled ZIP, and atomically renames the temporary file to the
requested destination.

The main and background EH workers pass
`EhentaiConfig.archive_download_concurrency` to the options-based client method.
They do not implement part scheduling themselves.

### Cache artifact layout

For a destination `{cache}/eh_cache/{gid}_{token}.zip`, the artifact family is:

```text
{gid}_{token}.zip                 final validated archive
{gid}_{token}.zip.part            legacy sequential partial or multipart assembly scratch
{gid}_{token}.zip.parts/
  manifest.json                   atomically replaced versioned manifest
  part-0000000000000000           append-only bytes for one interval
  part-0000000000000001
  ...
```

Part filenames use stable numeric IDs and do not encode mutable interval ends.
The manifest has this logical schema:

```text
version: 1
download_url: string
total_len: u64
etag: optional string
last_modified: optional string
next_part_id: u64
parts:
  - id: u64
    start: u64
    end: u64
```

Intervals are half-open `[start, end)`. Sorted intervals must be non-empty,
non-overlapping, gap-free, and cover exactly `[0, total_len)`. A part file stores
the contiguous prefix beginning at `start`; its file length must not exceed
`end - start`. Progress is derived from file metadata and is not duplicated in
the manifest.

The manifest is written to a sibling temporary file, flushed, and atomically
renamed into place. Structural updates never claim bytes that are only buffered
in memory.

## Download data flow

### Path selection

1. The existing archiver POST returns a download URL.
2. If a valid multipart manifest exists, remove any stale assembly scratch file
   and resume the manifest with at most the configured number of workers.
3. Otherwise, if a legacy `.zip.part` exists or maximum concurrency is one, use
   the existing contiguous-prefix downloader.
4. Otherwise, begin a new dynamic multipart transfer.

Multipart state takes precedence over `.zip.part` because the latter may be an
incomplete assembly left by a crash. Without a valid multipart directory, an
existing `.zip.part` retains its legacy sequential meaning.

### Starting a new multipart transfer

The first request is a single open-ended `Range: bytes=0-` request. No fixed
initial partition is created.

- A valid `206` supplies the total length through `Content-Range`. Create one
  interval `[0, total_len)`, persist the manifest, and stream into its part file.
- A `200` means the server ignored Range. Stream that response through the
  existing sequential path rather than issuing another full request.
- A `416` or an invalid initial `206` abandons multipart initialization and uses
  a fresh sequential request from byte zero.

Strong ETag is the preferred validator; Last-Modified is used when no strong
ETag is available. Validators are optional under the selected compatibility
policy.

### Rate sampling

Each active part tracks bytes durably handed to its file and elapsed time.
Throughput is sampled after at least one second with non-zero progress. The first
sample initializes the EWMA. Later samples use:

```text
ewma = 0.25 * current_sample + 0.75 * previous_ewma
```

The heuristic minimum remaining size for a connection with rate `r` is:

```text
adaptive_min_bytes = max(1 MiB, ceil(r * 15 seconds))
```

The one-MiB floor prevents very slow or noisy samples from creating excessive
tiny files. The 15-second target gives a newly created part enough expected work
to justify another HTTP request. These values are internal constants, not user
configuration.

### Dynamic splitting and work stealing

The coordinator starts with one active part and gradually expands toward the
configured maximum. It evaluates splitting after a fresh throughput sample and
whenever a part completes.

1. If no connection slot is available, do not split.
2. Among active incomplete parts with a valid rate sample, choose the one with
   the most remaining bytes.
3. Request that task to stop, wait for it to exit, discard any bytes that never
   left its in-memory buffer, and recompute its cursor from the part file length.
4. Estimate the new connection's rate as the median EWMA of sampled active
   connections. If no other sample exists, use the selected part's rate.
5. Compute the selected and new connections' adaptive minimum sizes. Split only
   when the unclaimed tail is at least the sum of those minima.
6. Allocate the tail in proportion to the two estimated rates so their expected
   completion times are equal. Clamp the allocation so each child receives at
   least its adaptive minimum.
7. Shorten the selected part's interval at the split point, add a new stable part
   ID for the tail, atomically replace the manifest, then launch both remaining
   ranges without exceeding the configured maximum.

Only the selected part is paused. Its existing file remains attached to the
shortened interval, and the new part starts empty at the split point. The
coordinator waits for a new sample or completion event before another split, so
one stale measurement cannot cascade immediately into a fixed set of parts.

When a part completes and no eligible tail is large enough, its connection slot
remains unused; the implementation does not create undersized work merely to
reach the configured maximum.

### Per-part requests and retries

For a part `[start, end)` with `downloaded = file_len`, request exactly:

```text
Range: bytes={start + downloaded}-{end - 1}
```

Every `206` must report the exact requested start and end and the manifest's
total length. When a strong ETag or Last-Modified validator is available, send
`If-Range` and require the response validator to match.

A truncated body or transient stream error preserves the part file. The same
part resumes from its new file length, with at most the existing four request
attempts and one-second retry delay. When a part exhausts its attempts, the
coordinator stops the archive transfer. Aggregate progress is the sum of all
part file lengths, so existing `DownloadInProgress` classification uses the
whole archive's durable byte delta without double counting.

## Recovery and consistency

On recovery, validate the manifest version, URL, total length, interval coverage,
part IDs, and every referenced file length before starting requests. Remove
unreferenced files inside a valid multipart directory. A malformed manifest,
missing required part file, impossible file length, gap, or overlap invalidates
the complete multipart state; do not infer missing structure.

When validators are present, a validator change, missing required validator, or
`If-Range` fallback invalidates all multipart bytes. Without validators, allow
cross-process recovery when URL and total length match and every response has a
valid `Content-Range`. This deliberately accepts the residual risk that an
object replaced with different bytes at the same URL and length cannot be
identified before final ZIP validation.

During an existing multipart transfer, any `200`, `416`, invalid `206`, URL
change, total-length change, or validator mismatch causes the coordinator to:

1. cancel and join every part task;
2. delete the multipart directory and assembly scratch file; and
3. restart once from byte zero through the sequential downloader.

Ordinary transport failures do not trigger this reset. If final ZIP or entry
validation fails, delete the assembly scratch and multipart directory and return
the validation error; the application worker's existing retry policy starts the
next attempt from zero.

## Assembly and cleanup

When all part files exactly fill their intervals, create a fresh `.zip.part` and
append part files in ascending `start` order. Flush the assembly, run existing
complete ZIP and entry validation, then atomically rename it to `.zip`. Delete
the multipart directory only after the final rename succeeds. A crash before
rename leaves the manifest and part files authoritative, so the assembly scratch
is discarded and rebuilt.

Introduce one shared archive-artifact helper that identifies or removes the
final ZIP, legacy partial, and multipart directory. Use it in:

- successful multipart cleanup;
- `EhDownloadWorker::cleanup_zip()` after permanent failure;
- `Repo::cleanup_eh_cache_orphans()` at startup.

The orphan cleaner treats `.zip.parts` directories as belonging to the same
logical `{gid}_{token}.zip` base. Active retryable queue entries preserve the
whole artifact family. Orphaned files and directories are removed recursively.

## Error handling and observability

- Do not log archive URLs, cookies, manifest contents, or token-bearing paths at
  normal levels.
- Include archive identity already used by existing worker logs and concise part
  IDs/ranges in diagnostic error context, without response bodies.
- Preserve the existing distinction between fast durable progress
  (`DownloadInProgress`) and ordinary retryable failure.
- Manifest I/O, part I/O, assembly, and validation errors propagate through the
  existing `eh_client::Error` conversion path; no database schema change is
  required.

## Testing

### Policy and manifest tests

- EWMA initialization and update formula.
- The one-MiB floor and 15-second adaptive minimum at low and high rates.
- Proportional split boundaries, minimum-size clamping, and largest-remaining
  candidate selection.
- No split without a stable sample, without a free slot, or when the tail is too
  small.
- Manifest round trip, atomic replacement, interval coverage, corrupt manifest,
  gaps, overlaps, duplicate IDs, missing files, oversized part files, and
  unreferenced-file cleanup.

### HTTP integration tests

Wiremock tests verify:

- a new multipart download begins with one open-ended Range rather than fixed
  initial chunks;
- measured progress dynamically creates additional bounded Range requests;
- active requests never exceed the per-archive maximum;
- completion of a part triggers another eligible split of the largest remaining
  interval;
- all successful ranges are non-overlapping, cover `[0, total_len)`, and assemble
  byte-for-byte to the source ZIP;
- process-style restart resumes persisted part offsets with a strong validator;
- restart without a validator resumes by URL, total length, and Content-Range;
- `200`, `416`, invalid Content-Range, URL change, total change, and validator
  change purge multipart state and use the sequential fallback;
- transient truncated responses preserve part progress and participate in the
  existing aggregate `DownloadInProgress` behavior;
- final ZIP and entry corruption removes multipart state.

### Application integration tests

- Configuration defaults to one, accepts values above one, and rejects zero.
- Main and background workers pass the configured maximum to the client.
- Successful completion, permanent failure, and orphan cache cleanup handle the
  entire artifact family while retaining active resumable state.
- Existing sequential Range-resume and unauthenticated image fallback tests stay
  green.

Run focused `eh_client` and EH scheduler/repository tests, then run `make ci` as
required for Rust changes. Wiremock is the real HTTP surface for concurrency and
resume verification; tests do not contact EH or spend GP.

## Accepted trade-offs

- Per-archive and queue-level concurrency multiply, but defaulting the new limit
  to one preserves current request volume until explicitly enabled.
- Independent part files require one sequential local assembly pass, chosen in
  exchange for simpler crash recovery and no concurrent random writes.
- Compatibility-mode recovery without validators cannot detect same-URL,
  same-length object replacement. Exact Range validation and final ZIP validation
  reduce but do not eliminate that risk.
- Dynamic rate-based splitting does not guarantee using every configured slot for
  small or nearly complete archives; avoiding undersized requests takes priority.

## Self-review

- Placeholder scan: no placeholders or deferred decisions.
- Internal consistency: configuration, dynamic splitting, manifest recovery,
  fallback, assembly, and cleanup describe the same artifact lifecycle.
- Scope: changes are limited to EH archive transfer, its configuration threading,
  cache cleanup, and direct tests.
- Ambiguity: defaults, invalid values, artifact names, manifest fields, interval
  semantics, rate formula, split trigger, retry behavior, fallback conditions,
  and cleanup ordering are explicit.
