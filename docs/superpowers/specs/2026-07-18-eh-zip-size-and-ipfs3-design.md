# EH archive size limit and ipfS3 ZIP upload design

## Context

EH archive downloads currently call `prepare_archive_download()` and then `download_archive_with_request()`. The latter starts with the `archiver.php` POST that can consume EH archive points, so any size limit that runs inside or after that call is too late. The queue only stores the final downloaded `file_size`; download-time metadata size must come from gallery metadata before the paid archive request.

EH Telegraph upload can either upload each image through `ImageUploader::upload_images_with_url_pairs()` or use the optional ipfS3 ZIP-first path. The first ZIP-first implementation is incorrect: it performs a normal S3 PutObject, treats the archive ETag as a directory CID, and constructs `{gateway}/{archive-cid}/{entry-path}`. ipfS3's documented extension instead requires a signed `decompress-zip` query and returns a `DecompressZipResult` XML body containing a distinct ETag/CID for every extracted entry. The target deployment is confirmed to implement that documented extension.

## Goals

1. Add a configurable EH archive size limit, defaulting to `300` MiB, that fails before any archive POST/download request when gallery metadata says the archive is larger than the limit. A value equal to the limit is allowed; a value greater than the limit is rejected. `0` disables the limit.
2. When EH already has a downloaded ZIP and the active uploader is configured for ipfS3 ZIP extraction, upload the ZIP once with `decompress-zip=<target-prefix>/`, parse the result XML, and build Telegraph image URL pairs from each extracted image entry's own CID. If ZIP extraction is not configured, existing per-image upload behavior remains unchanged.

## Non-goals

- Do not implement multipart ZIP upload; the cached EH ZIP is already available as one request body.
- Do not change non-EH image uploads.
- Do not change Telegram archive sending, Telegraph splitting, or delayed preview/public gateway rewrite behavior except for the source of the image URL pairs.

## Design

### Archive size gate

Extend `EhentaiConfig` with `max_archive_size_mb: u64`, default `300`. Add `max_archive_size_bytes() -> Option<u64>` that returns `None` for `0`, otherwise `mb * 1024 * 1024` with saturating arithmetic. Document the field in `config.toml.example` and note that it is based on EH gallery metadata size.

Before logged-in archive downloads, both `EhDownloadWorker` and `EhBackgroundDownloadWorker` will fetch or reuse EH gallery metadata and call a shared guard before `download_archive_with_request()`. If `gallery.filesize > max_archive_size_bytes`, the worker returns a normal failure with a user-readable message such as `EH gallery archive is too large: 350 MiB exceeds configured 300 MiB limit`. Because this happens before `download_archive_with_request()`, no archive POST is issued and no archive points are spent for oversized galleries.

The unauthenticated fallback path `download_gallery_images()` does not use EH archive points and does not have reliable archive metadata before image fetches, so it is not blocked by this archive-size gate. The existing weekly aggregate `download_rate_limit_gb` remains unchanged and still accounts for completed downloads.

### ipfS3 ZIP-extract upload capability

Keep the existing optional upload abstraction, `upload_zip_archive_with_url_pairs(input) -> Result<Option<Vec<TelegraphImageUrlPair>>>`. The default trait implementation returns `Ok(None)`, so existing Pixi, S3, Catbox, and unconfigured ipfS3 uploaders keep the current per-image path.

Keep the ipfS3 config flag `zip_extract_enabled`, default `false`. When enabled, `IpfS3Uploader` will:

1. Build the archive object key with the existing key-prefix/timestamp/hash scheme.
2. Derive an isolated extraction prefix from that key by removing `.zip` and appending `/`.
3. Clone its configured `rust-s3::Bucket`, add `decompress-zip=<extraction-prefix>` only to that clone, and upload the ZIP as `application/zip`. `rust-s3` includes the bucket's extra query parameters in the SigV4-signed request.
4. Request the default result form; it must not send `decompress-zip-result=false` because the entry results are required.
5. Parse the successful response body as `DecompressZipResult` XML. The archive-level ETag is retained only as archive metadata and is never used for image URLs.

The XML parser models `Entries/Entry` (`Key`, `ETag`, `Size`) and `Failures/Failure` (`EntryName`, `Code`, `Message`). A response with any reported failure is rejected so a partially extracted archive cannot be treated as a complete Telegraph upload. Extra successful entries that are not uploadable images may exist and are ignored.

For every ordered image entry supplied in `ZipArchiveUploadInput.entry_names`, the uploader computes the expected final S3 key as `extraction-prefix + entry-name`, finds the corresponding result entry, trims optional quotes from that entry's ETag, and constructs URL pairs as `{preview_gateway_or_gateway}/{entry-cid}` and `{gateway}/{entry-cid}`. Matching is by exact final key, not result order. Duplicate result keys use the last result, matching ipfS3's last-entry-wins PutObject semantics. Missing keys, empty entry CIDs, malformed XML, inconsistent declared counts, or duplicate requested image names are errors.

`EhUploadWorker` first collects uploadable image entry names from the ZIP in archive order. If the uploader returns `Some(url_pairs)` and the number of URL pairs matches the image entry count, the worker creates Telegraph pages from those pairs and skips per-image extraction/upload. If the method returns `None`, it falls back to the existing per-image flow. If the method returns an error or mismatched result while configured, the upload fails and follows existing retry/fallback-to-archive behavior.

## Data flow

1. Download worker claims an EH queue row and verifies chat/activity as today.
2. For logged-in archive mode, worker obtains gallery metadata size before calling `download_archive_with_request()`.
3. Oversized metadata fails the row before archive POST; allowed metadata proceeds to existing archive download and writes final `file_size` as today.
4. Upload worker sees the downloaded ZIP path.
5. Upload worker collects ordered image entry names.
6. If the uploader supports configured ZIP extraction, it sends one SigV4 PutObject with the `decompress-zip` query and parses `DecompressZipResult` XML.
7. The uploader matches final result keys to the requested image entries and restores ZIP image order while building URLs from each entry's own CID.
8. Telegraph page creation, rewrite metadata, publish, and archive fallback reuse existing code paths.

## Error handling

- Oversized galleries are treated as permanent download failures after the existing retry policy decides the row is exhausted; logs include gid/token/title context, while user-facing text stays concise.
- Missing or zero metadata size does not block downloads, because rejecting unknown sizes could create false failures.
- ZIP-upload capability errors are upload errors, not download errors. Existing upload retries apply; if retries are exhausted and `send_archive` is enabled, the archive fallback remains available.
- A non-2xx response, malformed/empty XML, nonzero `FailedCount`, a nonempty `Failures` list, inconsistent counts, a missing requested image key, an empty entry ETag/CID, or duplicate requested image names fails the ZIP upload.
- Gateway URLs contain only the extracted entry CID, so ZIP entry paths are used for exact response matching but are not embedded in public URLs.

## Testing

- Config tests for default `max_archive_size_mb = 300`, disabled `0`, and byte conversion.
- EH download worker tests that an oversized gallery fails before the archive POST mock is hit, and an exactly-at-limit gallery still posts/downloads.
- EH background download helper coverage for the same guard or a shared guard unit test if worker-level setup is too heavy.
- ipfS3 uploader request test proving the signed PutObject contains `decompress-zip=<derived-prefix>/`, uses `application/zip`, and parses the XML response.
- XML/result mapping tests for per-entry CIDs, response order independence, ignored non-image entries, last-result-wins duplicate keys, failure entries, declared count mismatches, missing requested keys, duplicate requested names, and empty entry CIDs.
- A regression assertion proving archive ETag/CID is never used to construct extracted image URLs.
- EH upload worker test with a ZIP-capable mock uploader proving the worker calls the ZIP method once, preserves image order, and does not call per-image upload.
- Focused commands: targeted `cargo test` for `config`, `eh_client::telegraph`/ipfS3, and `scheduler::eh_engine`, then `make quick` or `make ci` if environment dependencies are available.

## Self-review

- Placeholder scan: no placeholders or unresolved TODOs.
- Consistency: the size gate is explicitly before `download_archive_with_request()`; ZIP upload uses the documented query/XML contract and entry CIDs throughout.
- Scope: the correction is limited to the ipfS3 uploader, its direct dependency/config documentation, and focused tests; the EH worker interface remains stable.
- Ambiguity: the target deployment is confirmed to implement the documented `decompress-zip` protocol; `zip_extract_enabled = false` preserves behavior for deployments that do not.
