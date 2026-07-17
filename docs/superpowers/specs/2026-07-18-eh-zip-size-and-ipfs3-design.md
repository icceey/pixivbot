# EH archive size limit and ipfS3 ZIP upload design

## Context

EH archive downloads currently call `prepare_archive_download()` and then `download_archive_with_request()`. The latter starts with the `archiver.php` POST that can consume EH archive points, so any size limit that runs inside or after that call is too late. The queue only stores the final downloaded `file_size`; download-time metadata size must come from gallery metadata before the paid archive request.

EH Telegraph upload currently opens the downloaded ZIP, reads each image into memory, uploads each image through `ImageUploader::upload_images_with_url_pairs()`, and builds Telegraph pages from those image URLs. `IpfS3Uploader` already maps upload response ETags/CIDs into preview/public gateway URL pairs, but it only supports per-image uploads. Public ipfS3/IPFS Ninja docs confirm normal S3-compatible upload behavior and CID-in-ETag, but do not document a universal ZIP-extract API, so ZIP extraction must be an explicit configurable ipfS3 capability instead of a hard-coded assumption.

## Goals

1. Add a configurable EH archive size limit, defaulting to `300` MiB, that fails before any archive POST/download request when gallery metadata says the archive is larger than the limit. A value equal to the limit is allowed; a value greater than the limit is rejected. `0` disables the limit.
2. When EH already has a downloaded ZIP and the active uploader is configured for ipfS3 ZIP extraction, upload the ZIP once and derive Telegraph image URL pairs from the extracted ZIP entry paths. This avoids per-image upload requests. If ZIP extraction is not configured, existing per-image upload behavior remains unchanged.

## Non-goals

- Do not invent undocumented ipfS3 headers, endpoints, or response schemas.
- Do not change non-EH image uploads.
- Do not change Telegram archive sending, Telegraph splitting, or delayed preview/public gateway rewrite behavior except for the source of the image URL pairs.

## Design

### Archive size gate

Extend `EhentaiConfig` with `max_archive_size_mb: u64`, default `300`. Add `max_archive_size_bytes() -> Option<u64>` that returns `None` for `0`, otherwise `mb * 1024 * 1024` with saturating arithmetic. Document the field in `config.toml.example` and note that it is based on EH gallery metadata size.

Before logged-in archive downloads, both `EhDownloadWorker` and `EhBackgroundDownloadWorker` will fetch or reuse EH gallery metadata and call a shared guard before `download_archive_with_request()`. If `gallery.filesize > max_archive_size_bytes`, the worker returns a normal failure with a user-readable message such as `EH gallery archive is too large: 350 MiB exceeds configured 300 MiB limit`. Because this happens before `download_archive_with_request()`, no archive POST is issued and no archive points are spent for oversized galleries.

The unauthenticated fallback path `download_gallery_images()` does not use EH archive points and does not have reliable archive metadata before image fetches, so it is not blocked by this archive-size gate. The existing weekly aggregate `download_rate_limit_gb` remains unchanged and still accounts for completed downloads.

### ipfS3 ZIP-extract upload capability

Extend the upload abstraction with an optional ZIP archive method, for example `upload_zip_archive_with_url_pairs(input) -> Result<Option<Vec<TelegraphImageUrlPair>>>`. The default trait implementation returns `Ok(None)`, so existing Pixi, S3, Catbox, and unconfigured ipfS3 uploaders keep the current per-image path.

Add an ipfS3 config flag, `zip_extract_enabled`, default `false`. When enabled, `IpfS3Uploader` will upload the EH ZIP as one S3 object with content type `application/zip`, read the returned CID from the ETag using the same validation rules as image uploads, and construct URL pairs as `{preview_gateway_or_gateway}/{cid}/{zip_entry_path}` and `{gateway}/{cid}/{zip_entry_path}`. This assumes the configured ipfS3 deployment expands ZIP uploads into a directory-like CID where original entry paths are addressable under the returned root CID; deployments that do not support that behavior must leave the flag disabled.

`EhUploadWorker` will first collect the uploadable image entry names from the ZIP in archive order. If the uploader returns `Some(url_pairs)` from ZIP upload and the number of URL pairs matches the collected image entry count, the worker creates Telegraph pages from those pairs and skips per-image extraction/upload. If the method returns `None`, it falls back to the existing per-image flow. If the method returns an error or mismatched result while configured, the upload fails and follows existing retry/fallback-to-archive behavior.

## Data flow

1. Download worker claims an EH queue row and verifies chat/activity as today.
2. For logged-in archive mode, worker obtains gallery metadata size before calling `download_archive_with_request()`.
3. Oversized metadata fails the row before archive POST; allowed metadata proceeds to existing archive download and writes final `file_size` as today.
4. Upload worker sees the downloaded ZIP path.
5. Upload worker collects ordered image entry names.
6. If the uploader supports configured ZIP extraction, the ZIP is uploaded once and URL pairs are derived from the returned root CID plus ordered entry paths.
7. Telegraph page creation, rewrite metadata, publish, and archive fallback reuse existing code paths.

## Error handling

- Oversized galleries are treated as permanent download failures after the existing retry policy decides the row is exhausted; logs include gid/token/title context, while user-facing text stays concise.
- Missing or zero metadata size does not block downloads, because rejecting unknown sizes could create false failures.
- ZIP-upload capability errors are upload errors, not download errors. Existing upload retries apply; if retries are exhausted and `send_archive` is enabled, the archive fallback remains available.
- ZIP entry paths are URL-encoded by segment before constructing gateway URLs. Non-image entries are ignored exactly like the current per-image path.

## Testing

- Config tests for default `max_archive_size_mb = 300`, disabled `0`, and byte conversion.
- EH download worker tests that an oversized gallery fails before the archive POST mock is hit, and an exactly-at-limit gallery still posts/downloads.
- EH background download helper coverage for the same guard or a shared guard unit test if worker-level setup is too heavy.
- ipfS3 uploader tests for ZIP-extract URL pair generation, path encoding, disabled capability returning `None`, and CID extraction failures.
- EH upload worker test with a ZIP-capable mock uploader proving the worker calls the ZIP method once, preserves image order, and does not call per-image upload.
- Focused commands: targeted `cargo test` for `config`, `eh_client::telegraph`/ipfS3, and `scheduler::eh_engine`, then `make quick` or `make ci` if environment dependencies are available.

## Self-review

- Placeholder scan: no placeholders or unresolved TODOs.
- Consistency: the size gate is explicitly before `download_archive_with_request()` and ZIP upload is explicitly optional/configured.
- Scope: one implementation plan can cover config, upload abstraction, EH workers, examples, and tests.
- Ambiguity: undocumented ipfS3 extraction behavior is resolved by an explicit `zip_extract_enabled` assumption documented in config; the default preserves existing behavior.
