# ipfS3 ZIP fallback compatibility design

## Context

The existing ipfS3 ZIP upload path correctly sends a SigV4-signed `PutObject` with `decompress-zip=<prefix>`, parses `DecompressZipResult`, and builds gateway URLs from each extracted entry CID. The remaining problems are at the optional-optimization boundary: deterministic ZIP incompatibilities and valid partial extraction results currently become retryable upload failures even though `ImageUploader::upload_zip_archive_with_url_pairs()` already uses `Ok(None)` to select the existing per-image fallback.

The behavior is compared against ipfS3 `master` at commit `276de042b29030195349fe91ac5ae8e944dcd591`.

## Goals

1. Use ZIP upload only when the archive and requested entries are compatible with the confirmed ipfS3 implementation.
2. Fall back to the existing per-image upload path for deterministic incompatibilities and incomplete extraction of requested images.
3. Preserve retries for transport, authentication, server, and protocol errors.
4. Correct stale comments that still describe archive-root CID URLs.

## Non-goals

- Do not reuse partial ZIP results and upload only missing images.
- Do not alter EH queue states, retry counts, archive delivery fallback, or Telegraph page construction.
- Do not add configuration or attempt to delete archive and entry objects left by a partial ipfS3 operation.
- Do not change non-ipfS3 uploaders or ordinary ipfS3 image uploads.

## Design

### ZIP compatibility inspection

Keep `ZipArchiveUploadInput` unchanged. `IpfS3Uploader` will inspect the ZIP bytes it already receives before constructing a network request. The inspection uses the central-directory metadata exposed by the existing `zip` dependency plus each entry's local-header flags and compression method, because ipfS3 validates the local header while streaming.

The ZIP-first path is inapplicable and returns `Ok(None)` when any archive entry would be rejected by the confirmed ipfS3 implementation, including an entry that:

- has a non-UTF-8, empty, absolute, Windows-drive, backslash-containing, `.`-segment, or `..`-segment name;
- is encrypted;
- uses a central or local compression method other than Stored or Deflate;
- has mismatched central and local compression methods; or
- uses Stored compression with a data descriptor.

The optimization also returns `Ok(None)` when requested image names are duplicated. Raw entry names are preserved. The client must not replace `\` with `/`, because ipfS3 rejects backslash entry names and therefore cannot return the normalized key the client would request. Directory and non-image entries remain outside the requested-image list, but are still preflighted because ipfS3 parses the complete archive.

### Result classification

Keep the existing strict XML structural checks: non-2xx status, empty or malformed XML, wrong root, inconsistent declared counts, and empty CIDs remain errors.

After a structurally valid HTTP 200 result:

- Build the final-key-to-CID map from successful entries, retaining last-response-entry-wins behavior.
- If every requested image key has a non-empty CID, return URL pairs in requested archive order. Failures concerning only unrequested entries do not invalidate the requested result.
- If any requested image is absent from the successful entries, including when a corresponding failure is reported, return `Ok(None)` so `EhUploadWorker` runs the existing per-image path.
- Duplicate requested names are handled by preflight fallback, not by accepting ipfS3's last-writer-wins value for two Telegraph positions.

This creates three outcomes at the existing interface boundary:

1. `Ok(Some(url_pairs))`: ZIP optimization completed all requested images.
2. `Ok(None)`: ZIP optimization is deterministically unsuitable or incomplete; use per-image upload.
3. `Err(error)`: transport, service, or protocol failure; preserve retry behavior.

## Data flow

1. `EhUploadWorker` opens the downloaded ZIP and collects ordered raw names for uploadable images.
2. It passes the unchanged ZIP bytes and ordered names to the uploader.
3. `IpfS3Uploader` inspects every ZIP entry and returns `Ok(None)` without a PUT when preflight detects an ipfS3-incompatible archive or duplicate requested image name.
4. Otherwise it sends the existing signed ZIP PUT and parses the XML response.
5. Complete requested results become CID URL pairs; incomplete requested results become `Ok(None)`.
6. `EhUploadWorker` naturally continues into its existing extraction and per-image upload loop when it receives `None`.

## Error handling

- Authentication errors, network errors, non-2xx responses, malformed XML, count mismatches, and impossible response shapes remain `Err` and use existing retries.
- ipfS3-incompatible ZIP entries anywhere in the archive and duplicate requested image names return `Ok(None)` before any ZIP PUT.
- Requested-entry extraction failures or missing requested keys in a valid 200 response return `Ok(None)`.
- Failures for unrequested files are ignored only when every requested image has a valid CID.
- Partial server-side objects are accepted as an ipfS3 protocol side effect; no cleanup API is available in this scope.

## Testing

- Unit tests for preflight classification: unsafe/non-UTF-8 names, duplicate requested names, encryption, unsupported or mismatched compression, Stored plus data descriptor, and supported Stored/Deflate entries.
- Transport test proving preflight fallback sends no ZIP PUT.
- Result tests proving an unrequested failure can still succeed, while a missing or failed requested image returns fallback.
- Existing malformed XML, count mismatch, non-success status, signed query, ordering, and entry-CID URL tests remain green.
- EH worker test proving `Ok(None)` continues to per-image upload and reaches uploaded state.
- Run focused tests followed by `make ci`.

## Self-review

- Placeholder scan: no deferred decisions or incomplete requirements.
- Internal consistency: compatibility failures and incomplete requested results use the existing `Ok(None)` contract throughout.
- Scope: changes are limited to private ipfS3 ZIP preflight/result classification, preserving raw EH entry names, comments, and focused tests; public interfaces remain unchanged.
- Ambiguity: duplicate entries fall back rather than silently overwrite; partial successful CIDs are not reused; service and protocol failures remain retryable errors.
