# S3/ipfS3 resumable multipart upload design

## Context

`S3Uploader` and ordinary `IpfS3Uploader` image uploads currently send each object with one `PutObject`. The ipfS3 ZIP-first path also sends the complete archive with one `PutObject` carrying `decompress-zip=<prefix>`. EH upload retries return the queue row to `downloaded`, but no upload session or acknowledged part state survives the failed attempt, so an interrupted object is transmitted again from byte zero.

The configured `rust-s3 0.37.2` dependency exposes CreateMultipartUpload, UploadPart, CompleteMultipartUpload, and AbortMultipartUpload helpers. It does not expose ListParts. Its `presign_get()` API can nevertheless sign a standard `GET key?uploadId=...` request, allowing the existing `reqwest` and `quick-xml` dependencies to implement paginated ListParts without adding the AWS SDK.

The confirmed ipfS3 implementation at commit `cd88a1734c871d3c2506f71b416fddebba848d69` supports multipart uploads and persists `decompress-zip` options when CreateMultipartUpload is called. Its later UploadPart, ListParts, Complete, and Abort requests must not repeat those extension queries. Complete retrieves the persisted ZIP options by upload ID and returns `DecompressZipResult` with each extracted entry's CID.

## Goals

1. Give both S3 and ipfS3 uploads S3-compatible, single-object multipart resume across EH task retries and process restarts.
2. Reconcile upload progress with the server through ListParts rather than trusting a local list of acknowledged parts.
3. Let ipfS3 ZIP uploads use multipart by default when both multipart and multipart ZIP extraction are supported.
4. Use multipart for images only when their byte length reaches a configurable threshold, defaulting to 8 MiB.
5. Preserve the existing complete-object PutObject paths as compatibility fallbacks only when the endpoint explicitly does not implement the required multipart operation.
6. Preserve the current `decompress-zip` request, response, fallback, ordering, and per-entry CID semantics.

## Non-goals

- Do not persist successful URLs for earlier images in a multi-image EH task. Resume is scoped to one ZIP or one image object.
- Do not add parallel part uploads.
- Do not add an AWS SDK, database migration, general upload queue, SSE support, or server-side orphan-session sweeper.
- Do not use HTTP OPTIONS for capability discovery. S3 OPTIONS is a CORS preflight and does not advertise multipart operations.
- Do not change Telegraph page splitting, preview/public gateway rewriting, EH archive downloading, or Telegram notification behavior.

## Configuration

Add `multipart_image_threshold_mb: u64` to both `S3UploaderConfig` and `IpfS3UploaderConfig`.

- Default: `8`.
- `0`: disable multipart for images only; enabled ipfS3 ZIP-first multipart remains controlled by `zip_extract_enabled`.
- An image uses multipart when `bytes.len() >= threshold_mb * 1024 * 1024`.
- Conversion uses saturating arithmetic.
- The multipart part size is fixed at 8 MiB. Every non-final part therefore exceeds S3's 5 MiB minimum.

Keep `IpfS3UploaderConfig::zip_extract_enabled` as an explicit operator opt-out, but change its default to `true`. When it is true, ZIP uploads always attempt the auto-detected multipart ZIP path regardless of archive size. When false, the uploader skips ZIP-first behavior and uses the existing per-image path.

Document the defaults, `0` behavior, lazy capability detection, and PutObject fallback in `config.toml.example`.

Multipart resume requires permission for CreateMultipartUpload, UploadPart, ListParts (`s3:ListMultipartUploadParts` on AWS S3), CompleteMultipartUpload, and AbortMultipartUpload. HEAD is used only to resolve an ambiguous lost Complete response. Document these permission requirements; a permission failure is an operator/configuration error, not evidence that the protocol is unsupported.

## Architecture

### Shared multipart module

Add a private `eh_client::s3_multipart` module with three focused units:

- `mod.rs`: the Create, reconcile, upload-missing-parts, Complete, and Abort state machine;
- `manifest.rs`: session identity validation and atomic JSON persistence;
- `list_parts.rs`: pre-signed ListParts requests, pagination, XML parsing, and S3 error parsing.

`S3Uploader` and `IpfS3Uploader` retain provider-specific object-key, public-URL, CID, gateway warmup, and ZIP-result behavior. They delegate byte transfer and resume to the shared multipart engine.

### Resume context

Extend `ImageUploadInput` and `ZipArchiveUploadInput` with an optional resume context containing a stable manifest path and logical object ID. Direct callers and tests may pass no context; multipart still works for the current call, but cross-call recovery requires the context supplied by `EhUploadWorker`.

`EhUploadWorker` derives contexts from the downloaded archive family:

- ZIP: `.zip.uploads/archive.json`;
- image: `.zip.uploads/image-<archive-order-index>.json`.

The image index is the original uploadable-entry order, not a per-attempt batch index. The object fingerprint prevents a stale manifest from being reused if an entry's content changes.

### Manifest

The versioned manifest contains only:

- provider kind;
- uploader identity fingerprint covering provider, endpoint, region, bucket, and path-style mode without credentials;
- stable object key;
- logical object ID;
- SHA-256 object fingerprint and byte length;
- content type and fixed part size;
- multipart upload ID;
- optional ipfS3 ZIP extraction prefix and requested-entry-name fingerprint.

It never contains credentials, pre-signed URLs, object bytes, or locally asserted completed-part ETags. Add the small `sha2` dependency to `eh_client` so resume identity is collision-resistant.

Write the manifest atomically immediately after CreateMultipartUpload succeeds. Use the existing tempfile, flush, `sync_all`, and persist pattern from the archive download manifest. If persistence fails, attempt AbortMultipartUpload before returning the error.

`ArchiveArtifacts` gains a separate `.zip.uploads` directory and a dedicated `remove_upload_state()` operation. The existing download multipart cleanup must continue to remove only `.zip.part` and `.zip.parts`; in particular, startup orphan cleanup must preserve `.zip.uploads` while its final ZIP belongs to an active or retryable queue row. A durable cleanup gate applies before any local upload-state or whole-family deletion: derive `.zip.uploads` from the entry ZIP, delete normally when it is absent, but when it exists require the configured S3/ipfS3 Abort uploader to deterministically scan both direct `*.json` manifests and `manifest.json.tmp-*` atomic-write remnants, then Abort every safely identifiable matching session successfully. A crash can leave either a complete or partial temporary manifest after write/flush/`sync_all` and before `persist`; an unverifiable temporary manifest must return a sanitized error so the gate retains the complete local family and its sole upload ID. No uploader or an Abort error also retains the complete family. This gate covers successful Telegraph cleanup, upload cancellation/permanent failure, publish completion/permanent failure/cancellation, startup orphan cleanup, and download/background cleanup; publish performs it before marking the row done. `NoSuchUpload` is already a successful provider Abort result.

### Capability state

Each S3-backed uploader keeps process-local capability states:

- standard multipart: unknown, supported, or unsupported;
- ipfS3 multipart ZIP extraction (Create/Complete): unknown, supported, or unsupported;
- ipfS3 single-Put ZIP extraction: unknown, supported, or unsupported.

No separate probe object is created. For an eligible real object, CreateMultipartUpload creates the actual session. An immediate empty ListParts request verifies that the endpoint supports the server reconciliation required by this design. The same session then uploads data.

Create plus ListParts establishes provisional standard multipart support. A successful Complete establishes full support. For ipfS3 ZIP, only a valid `DecompressZipResult` establishes multipart ZIP-extraction support.

## Standard multipart flow

1. Select multipart for an eligible image or for an enabled ipfS3 ZIP.
2. Load and validate the local manifest against provider, bucket, logical ID, content type, byte length, SHA-256 fingerprint, part size, and ZIP option fingerprints.
   A parseable manifest whose provider or uploader identity differs is not a stale session that this uploader may replace: retain it and return the fixed sanitized resume-identity error before making any network request. The fingerprint covers provider, endpoint, region, bucket, and path-style mode, so an operator must restore the matching configuration or use provider lifecycle/manual resolution; the sole object key and upload ID must not be discarded.
3. If no valid manifest exists, generate the provider's object key once, initiate multipart, and atomically persist the returned upload ID and session identity.
4. Request every ListParts page using a short-lived URL produced by `Bucket::presign_get()` with `uploadId`, `part-number-marker`, and `max-parts`. Never persist or log the signed URL. Strip the URL from `reqwest::Error` before wrapping it so credentials and signatures cannot enter logs or queue error text.
5. Validate the response bucket, key, upload ID, pagination markers, unique part numbers, non-empty ETags, and each part's expected byte size. The service response is authoritative.
6. Skip server parts whose number and size match the fixed 8 MiB partitioning. Upload every missing or uncertain part with its deterministic part number. Re-uploading the same part number safely replaces an accepted part whose response was lost.
7. Build the CompleteMultipartUpload part list from the reconciled and newly returned ETags, sorted strictly by part number.
8. Complete the upload and process the provider-specific response.
9. Remove the manifest only after the response has been fully parsed and the final URL data has been produced.

At most 10,000 parts are allowed. An object that would exceed this limit fails before CreateMultipartUpload.

## ipfS3 `decompress-zip` compatibility contract

Multipart ZIP extraction is a separate extension capability layered on standard multipart.

### Request contract

Create uses a dedicated clone of the configured Bucket and adds the extension queries only to that clone:

```text
POST /bucket/archive.zip?uploads&decompress-zip=<extraction-prefix>
```

The client requires the default `decompress-zip-result=true` behavior and does not send SSE headers.

All later requests use the unmodified base Bucket and must not carry any `decompress-*` query:

```text
PUT    /bucket/archive.zip?partNumber=N&uploadId=ID
GET    /bucket/archive.zip?uploadId=ID
POST   /bucket/archive.zip?uploadId=ID
DELETE /bucket/archive.zip?uploadId=ID
```

This is mandatory because current ipfS3 interprets a PUT containing `decompress-zip` as the single-object ZIP route; adding the query to UploadPart would misroute a part as an entire archive.

The extraction prefix and requested entry-name fingerprint are persisted in the manifest. A resumed object must reproduce both exactly. Complete sends the exact CID ETags returned by UploadPart or ListParts in ascending part order; it does not treat them as MD5 hashes.

### Completion contract

ipfS3 restores the ZIP option from the multipart upload record identified by `uploadId`. A successful Complete must return a structurally valid `DecompressZipResult`. Reuse the existing strict parser and result classification:

- archive ETag/CID is archive metadata only;
- every requested image entry must resolve to a non-empty entry ETag/CID;
- output URL pairs preserve requested archive order;
- deterministic archive or result incompatibility returns `Ok(None)` to the existing per-image fallback;
- transport, authentication, server, and malformed-protocol failures remain errors.

If multipart Complete succeeds but returns a standard `CompleteMultipartUploadResult`, standard multipart remains supported but multipart ZIP extraction is marked unsupported for the process. Remove the completed session manifest, then use the existing single PutObject `decompress-zip` path to obtain `DecompressZipResult`. This may leave the just-completed archive object as a compatibility side effect; deleting it is outside scope because delete permission is not currently required.

## Provider results

### S3

After successful Complete, return `public_base_url + object_key` exactly as the current PutObject path does.

### ipfS3 image

Read the completed object's CID from the standard CompleteMultipartUpload result ETag, accepting quoted CID ETags and not applying MD5 validation. Build preview/public gateway URLs and perform the existing optional public-gateway warmup.

### ipfS3 ZIP

Parse the `DecompressZipResult` body and build URLs from each extracted entry CID. Never build image URLs from the multipart root/archive CID.

## Error handling and fallback

For an enabled ipfS3 ZIP single PutObject request, a 2xx response whose body is empty or wholly ASCII whitespace is a compatibility signal: a standard S3 endpoint accepted the archive but does not implement `decompress-zip`. The only other downgrade signals are a strictly parsed, standalone S3 `<Error>` containing `NotImplemented` on a non-5xx status or canonical HTTP 501, `UnsupportedOperation` on a non-5xx status, or `MethodNotAllowed` on HTTP 405. Each marks only the process-local ZIP-extraction capability unsupported and returns `Ok(None)` so the scheduler uses its existing ordered per-image fallback. HTTP 500 `NotImplemented`, other 5xx, raw or malformed 501, malformed `<Error>`, authentication failures, non-empty unknown/malformed success bodies, and transport failures remain errors; none may be silently downgraded.

For Create, ListParts, UploadPart, and Complete, a strictly parsed S3 `NotImplemented` code in a non-5xx error response, including a Complete HTTP 200 `<Error>` response, may mark that operation unsupported. Canonical HTTP 501 with that code is the allowed 5xx exception. Explicit `UnsupportedOperation` or an operation-specific `MethodNotAllowed` response remain fallback signals. HTTP 500 with `NotImplemented` and every other 5xx response remain errors; raw or malformed 501, mismatched operations, Head or Abort, a raw OPTIONS result, generic 4xx status, authentication error, network error, timeout, or malformed response must not silently trigger PutObject fallback.

If an operation is explicitly unsupported:

- for an active session with a persisted manifest, require a successful Abort (with `NoSuchUpload` treated as successful cleanup) before removing the matching manifest and returning the fallback signal; an Abort network, IAM, or 5xx failure returns an error and retains the manifest;
- for an active session without a manifest, make the existing best-effort Abort because no resumable state exists;
- cache only the affected capability as unsupported;
- retry that object through the existing complete PutObject path.

If ListParts returns `NoSuchUpload`, remove the stale manifest and create one replacement session in the current call. A second invalid-session result is returned as an error, preventing loops.

If the manifest is valid but ListParts reports duplicate, out-of-range, or wrongly sized parts, attempt Abort, clear the manifest, and create one replacement session. Malformed local JSON is removed; when its upload ID cannot be recovered, remote cleanup is impossible.

For a normal resume, a parseable manifest with `Provider` or `UploaderIdentity` mismatch returns a fixed error without deleting the manifest or issuing Create, ListParts, UploadPart, Complete, Abort, or HEAD against the current endpoint. This keeps the persisted object key and upload ID available for a restored matching configuration, provider incomplete-multipart lifecycle policy, or manual resolution. It is distinct from a same-uploader stale manifest, which keeps the existing confirmed-Abort-before-replacement rule.

If Complete may have succeeded but its response was lost, `NoSuchUpload` confirms the persisted upload ID is dead. Only S3 and ipfS3 image recovery then HEAD the stable object key:

- S3 may accept a matching object length as completed and return its public key URL;
- ipfS3 image additionally requires a non-empty CID ETag;
- `HeadRecovery::Never` for ipfS3 ZIP sends no HEAD because archive metadata cannot recover per-entry CIDs; it directly removes the dead manifest, consumes the call's one replacement budget, and starts a replacement ZIP session to obtain a complete extraction response.

For the recovery strategies that actually issue HEAD, a forbidden or malformed response is returned as an error rather than guessed as absence or success. A confirmed missing object proceeds to the one allowed replacement session.

Known active sessions are aborted when they are deliberately replaced. The shared cleanup gate deterministically scans direct `.zip.uploads/*.json` manifests and `manifest.json.tmp-*` atomic-write remnants with the configured provider-specific S3/ipfS3 uploader; `NoSuchUpload` is already clean, and one failure does not stop later Abort attempts. An Abort error blocks deletion so the manifests retain the sole upload IDs for a later retry or startup cleanup. `main.rs` constructs one EH image uploader when EH is enabled and either Telegraph is configured or the provider is S3/ipfS3; its S3/ipfS3 Abort capability is cloned from that same `Arc` into startup, upload, and publish cleanup. Therefore an S3/ipfS3 Abort uploader exists without a Telegraph client/token, while Pixi/Catbox without Telegraph and every provider with EH disabled pass `None` and cannot use the trait default no-op as a success signal. A malformed formal JSON manifest keeps its existing skip behavior. A formal manifest that parses but cannot be safely verified because its version is unsupported, its stored values are invalid, or its provider or uploader identity does not match is never aborted against the current endpoint; terminal cleanup returns its fixed sanitized `Unverifiable` error and retains the complete family while continuing to Abort other independently verified manifests. An unrecoverable atomic temporary manifest is likewise never aborted, but it returns the existing sanitized temporary-manifest error that blocks deletion of the whole family because it may contain the only persisted upload ID. Those remote sessions cannot be safely discovered without ListMultipartUploads and remain subject to the object store's incomplete-multipart lifecycle policy.

Startup, upload, and publish cleanup follow this same rule across owner changes and Telegraph disablement: before removing a genuine orphan, canceled family, or archive-only publish family with `.zip.uploads`, the gate calls `abort_upload_state()` while that directory still exists and removes it only after success. Without an Abort-capable configured uploader, or when Abort fails, it logs/returns a safe gid-only error and preserves the entire family. Families without upload state retain existing cleanup behavior; active and retryable families continue to preserve upload state. When upload fallback Abort fails, its dedicated CAS permits only `uploading -> downloaded`; it rejects `publishing` rather than changing another stage. A publish cleanup-gate failure instead uses a dedicated publishing-only, no-increment CAS to release `publishing -> uploaded` when Telegraph exists or `publishing -> downloaded` otherwise; it preserves sent markers, does not consume the publish retry budget, and lets the next claim retry cleanup without resending.

## Testing

All default tests remain offline with `wiremock`.

### Configuration and selection

- S3 and ipfS3 thresholds default to 8 MiB; `0` disables image multipart.
- A `threshold - 1` image uses one PutObject; an exactly-threshold image uses multipart.
- `zip_extract_enabled` defaults true and explicit false preserves per-image behavior.
- EH uploader wiring constructs no uploader while EH is disabled; with EH enabled it constructs every provider when Telegraph is available, and without Telegraph it constructs only S3/ipfS3 so the same `Arc` supplies startup/upload/publish Abort cleanup.

### Standard resume

- Simulate a three-part image whose third part fails. Reconstruct the uploader, return the first two parts from ListParts, and prove only part three is uploaded before Complete.
- Simulate an UploadPart response loss where ListParts later reports the accepted part; prove it is not retransmitted.
- Cover paginated ListParts, quoted ETags, sorted Complete parts, duplicate/out-of-range parts, wrong sizes, stale upload IDs, malformed manifests, and the one-session-replacement limit.
- Prove signed URLs and credentials never appear in persisted state or errors.

### Capability and errors

- Strictly parsed `NotImplemented` for Create, ListParts, UploadPart, or Complete in a non-5xx S3 error response, including a Complete HTTP 200 `<Error>` response, or in canonical HTTP 501, aborts when possible and uses exactly one PutObject.
- Explicit `UnsupportedOperation` or operation-specific `MethodNotAllowed` also cleans up and falls back without disabling unrelated capabilities.
- HTTP 500 `NotImplemented`, every other 5xx, and malformed XML failures remain retryable errors and do not use PutObject fallback; raw or malformed 501 and Head or Abort `NotImplemented` do not downgrade.
- HTTP OPTIONS is never sent.

### ipfS3 ZIP compatibility

- Create contains signed `uploads` and `decompress-zip`; UploadPart, ListParts, Complete, and Abort contain no `decompress-*` query.
- Resume preserves the exact extraction prefix and requested-entry identity.
- Complete without repeated extension queries still returns and parses `DecompressZipResult`.
- Part ETags are CID strings and are submitted unchanged except for XML escaping.
- Archive CID is never used for image URLs; entry order and existing partial-result fallback remain unchanged.
- A standard CompleteMultipartUpload result marks only multipart ZIP extraction unsupported and falls back to the existing single PutObject ZIP extension.
- A default-enabled ZIP single PutObject that returns `200` plus an ETag but an empty/ASCII-whitespace body, canonical `501 NotImplemented`, non-5xx `UnsupportedOperation`, or `405 MethodNotAllowed` returns `Ok(None)`, caches only single-Put ZIP extraction as unsupported, and reaches per-image fallback. Multipart ZIP and single-Put ZIP extraction caches remain independent: a multipart ZIP failure may still make one compatible single-Put attempt, while a cached single-Put unsupported result makes later ZIP calls return `Ok(None)` with no network request. HTTP 500 `NotImplemented`, raw/malformed 501, malformed `<Error>`, and malformed non-empty success XML still error.

### Lifecycle and regressions

- Every successful, canceled, permanent-failure, orphan, archive-only, or download/background cleanup that would delete `.zip.uploads` first passes the same S3/ipfS3 Abort gate. A missing uploader or terminal Abort failure preserves the local family and manifests for retry or startup cleanup; publish releases a gate failure only through its publishing-only, no-increment defer, so marker-written rows remain ready for cleanup without resending regardless of the configured retry limit. Active/retryable rows continue to preserve upload state while startup cleanup may independently remove stale download `.zip.parts` state.
- Existing S3/ipfS3 PutObject, ZIP preflight, Telegraph creation, preview rewrite, Pixi, and Catbox tests remain green.

Run focused `eh_client` telegraph/multipart tests and EH scheduler/repository cleanup tests, then run `make ci`.

## Acceptance scenarios

1. **Interrupted large image:** after two acknowledged 8 MiB parts and process restart, ListParts reports both parts, no request resends them, the remaining part completes, and the final URL matches the provider's existing semantics.
2. **Interrupted ipfS3 ZIP:** Create carries `decompress-zip`, resumed part operations do not, Complete returns every requested entry CID, and Telegraph receives URLs in archive order.
3. **Unsupported endpoint:** an explicit multipart-not-supported error causes one complete PutObject fallback, while authentication and transient failures do not.
4. **Adjacent regression:** small images, explicit ZIP disablement, non-S3 providers, download resume artifacts, and Telegraph publishing preserve their current behavior.

## Self-review

- Placeholder scan: no deferred decisions or incomplete requirements.
- Internal consistency: ListParts is server-authoritative, while the local manifest stores only the session identity needed to call it; active-row startup cleanup preserves that manifest.
- Scope: resume is one object at a time; task-level successful-image persistence and unrelated upload features are excluded.
- Ambiguity: ZIP extension queries, image threshold boundary, unsupported-error fallback, stale-session replacement, and completion-result handling are explicit.
- Confirmed review fixes: empty successful single-Put ZIP responses have the sole new downgrade path, and startup orphan cleanup's uploader availability and local-deletion order are explicit.
- Confirmed terminal-cleanup fix: malformed formal JSON remains skippable, but parseable manifests whose version, stored values, provider, or uploader identity cannot be verified return only a fixed sanitized error and retain the family within the terminal-cleanup boundary. Separately, normal resume with a provider or uploader-identity mismatch also retains its manifest and returns its fixed sanitized resume error without contacting the current endpoint.
