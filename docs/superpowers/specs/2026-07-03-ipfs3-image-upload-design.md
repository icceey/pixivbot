# ipfS3 Image Upload Provider Design

## Context

`eh_client::telegraph` already supports three image upload providers — `pixi` (default), `s3`, and `catbox`. The existing `S3Uploader` uploads objects through S3-compatible `PutObject` and returns public URLs built from `public_base_url + object_key`. That URL model assumes the backing store is a plain CDN/object store that serves objects by key.

ipfS3 (https://github.com/hugefiver/ipfS3) is an S3-compatible gateway in front of IPFS (Kubo). It accepts standard S3 `PutObject` calls, but it returns the IPFS Content ID (CID) of the uploaded object in the response `ETag` header instead of an MD5 digest. Once an object is pinned to IPFS, it is reachable through any public IPFS gateway at `{gateway_origin}/ipfs/{CID}`, independent of the ipfS3 endpoint that received the upload. This decouples the upload destination from the public read path and lets operators publish images through durable, content-addressed URLs.

The requested change adds ipfS3 as a fourth image upload provider that reuses the S3 upload transport but derives public URLs from the response `ETag` (CID) instead of from the object key.

## Goals

1. Add `ipfs3` as a fourth `ImageUploadProvider` variant alongside `pixi`, `s3`, and `catbox`.
2. Reuse the S3-compatible `PutObject` transport (same `rust-s3` crate, same path-style and credential handling) for uploading objects to ipfS3.
3. Read the IPFS CID from the `ETag` header of each `PutObject` response and return `{gateway_url}/{CID}` as the public URL.
4. Keep the configuration shape consistent with the existing `S3UploaderConfig`: explicit endpoint, bucket, region, credentials, optional key prefix, optional path-style flag.
5. Keep the default `pixi` provider unchanged and avoid any regression in existing `S3Uploader`/`CatboxUploader` behavior.
6. Avoid logging credentials; never embed access keys or the S3 endpoint in returned public URLs.
7. Keep default tests offline and deterministic.

## Non-Goals

- Do not introduce presigned URL generation.
- Do not implement ipfS3-specific encryption headers (SSE-S3 / SSE-C). Objects are uploaded as plaintext so they remain gateway-accessible.
- Do not implement multipart upload; images are already bounded by the existing 6 MiB extraction limit.
- Do not add bucket creation, ACL management, or IPFS pin management beyond what ipfS3 performs internally.
- Do not make real ipfS3/S3 network calls in CI.
- Do not change Telegraph page creation semantics, EH upload worker state machine, or any other provider.
- Do not refactor `S3Uploader` or extract a shared S3 base type. The small overlap (bucket construction, object key generation) is accepted as coincidental repetition.

## Configuration

Add a new optional `[image_upload.ipfs3]` block. The top-level `provider` value gains an `ipfs3` option:

```toml
[image_upload]
# Default: "pixi". Also supports "s3", "catbox", "ipfs3".
provider = "pixi"

[image_upload.ipfs3]
endpoint_url = "http://localhost:9000"
bucket = "pixivbot-images"
region = "us-east-1"
access_key_id = "test"
secret_access_key = "test"
# Public IPFS gateway prefix. The CID returned by ipfS3 is appended after a "/".
# Example: "https://ipfs.io/ipfs" -> "https://ipfs.io/ipfs/<CID>"
gateway_url = "https://ipfs.io/ipfs"
# Optional S3 object key prefix; only organizes storage, does not affect public URL.
key_prefix = "eh"
# Optional; defaults to true for compatibility with ipfS3 path-style S3 API.
path_style = true
```

Rules:

- `provider = "ipfs3"` requires every ipfS3 field except `key_prefix` (defaults to empty) and `path_style` (defaults to `true`).
- `endpoint_url` and `gateway_url` are validated with the existing `validate_http_url` helper: must be `http` or `https`, no userinfo, no query, no fragment.
- `gateway_url` is trimmed of trailing slashes before URL generation. The generated public URL is `gateway_url + "/" + CID`.
- `endpoint_url` and `gateway_url` are independent: the S3 API endpoint that receives uploads need not be the same host as the public IPFS gateway that serves reads.
- Secrets may also be overridden through the existing config environment mechanism, e.g. `PIX__IMAGE_UPLOAD__IPFS3__SECRET_ACCESS_KEY`.

## Architecture

Introduce a new `IpfS3Uploader` alongside the existing `S3Uploader`, implementing the same `ImageUploader` trait:

```rust
#[async_trait]
pub trait ImageUploader: Send + Sync {
    async fn upload_images(&self, images: &[ImageUploadInput<'_>]) -> Result<Vec<String>>;
}
```

Concrete additions:

- `IpfS3UploaderConfig`: deserialized config with the fields above.
- `ResolvedIpfS3UploaderConfig`: validated, resolved internal struct (mirrors `ResolvedS3UploaderConfig`).
- `IpfS3Uploader { bucket: Box<Bucket>, config: ResolvedIpfS3UploaderConfig }`: holds the configured `rust-s3` bucket and resolved config.

The `ImageUploadProvider` enum gains an `IpfS3` variant; `ImageUploadConfig` gains an `ipfs3: Option<IpfS3UploaderConfig>` field; `build_uploader()` matches `IpfS3` and constructs the uploader.

`IpfS3Uploader` reuses the existing shared helpers already used by `S3Uploader`:

- `detect_content_type` (magic-byte detection),
- `extension_for_upload` (extension from content type or filename),
- `safe_upload_filename` (sanitized filename segment),
- `short_hash_hex` (FNV-1a 32-bit hash),
- `object_key` generation with the same `{key_prefix}/{yyyyMMddHHmmss}-{index:04}-{hash8}.{ext}` shape.

The object key is still generated and used as the S3 key for upload, but it is **not** used to build the public URL. The public URL is derived solely from the response `ETag` (CID) and `gateway_url`.

## Upload and CID-based URL Generation

`IpfS3Uploader::upload_images` flow per image:

1. Detect content type from magic bytes; derive extension.
2. Generate the S3 object key as `{key_prefix}/{timestamp}-{index:04}-{hash8}.{ext}` (same shape as `S3Uploader`).
3. Call `bucket.put_object_with_content_type(&key, bytes, &content_type)`.
4. Check `response.status_code()` is in `200..300`; otherwise return an error.
5. Read the ETag from the response body via `response.as_str()`. In rust-s3 0.37, `put_object_with_content_type` calls `response_data(true)`, which moves the `ETag` header out of the headers map into the response body (the documented access path for PUT ETags in this crate). Reading `response.headers().get("etag")` would return `None`.
6. If the body is empty or yields an empty/whitespace-only ETag, return an error — without the CID the public URL cannot be constructed.
7. Strip surrounding double quotes from the ETag value (S3 ETags are quoted, e.g. `"bafybei…"`).
8. Build the public URL as `{gateway_url_trimmed}/{cid}`.
9. Push the URL into the result vector.

CID encoding notes:

- IPFS CIDs (v0 `bafybei…`, v1 `bafkrei…` etc.) contain only `[a-z0-9]`, so no percent-encoding is required when appending to the gateway URL.
- The ETag value is used verbatim after quote-stripping; no CID format validation is performed, because ipfS3 guarantees the ETag is the CID for plaintext uploads and adding format checks would couple this code to a specific CID version.

## Error Handling

- Config validation fails at startup if `provider="ipfs3"` and any required ipfS3 field is missing or any URL fails `validate_http_url`.
- A `PutObject` failure (non-2xx status) returns an error to `EhUploadWorker`; existing retry/fallback logic handles transient and permanent failures, identical to `S3Uploader`.
- A successful `PutObject` with a missing or empty `ETag` header returns an error. This is treated as an upload failure because the public URL cannot be derived.
- If some images succeed and a later image fails, the whole Telegraph upload attempt is treated as failed, matching the existing `S3Uploader` policy. Object cleanup is not attempted; ipfS3-pinned objects are content-addressed and can be garbage-collected through Kubo lifecycle policies.
- Logs may include bucket name, endpoint host, object key, and the derived CID, but must not include access key ID or secret access key.

## Dependencies

No new crate dependencies. `IpfS3Uploader` reuses the existing `rust-s3` (`durch/rust-s3` 0.37) dependency already used by `S3Uploader`, including:

- `s3::creds::Credentials` for explicit access key / secret key,
- `s3::region::Region::Custom` for custom endpoint,
- `s3::Bucket::new` with optional `with_path_style()` for path-style addressing,
- `Bucket::put_object_with_content_type` returning `ResponseData` with `status_code()` and `as_str()` accessors (the latter exposes the ETag after it is moved into the body by `response_data(true)`).

`reqwest`, `async-trait`, `chrono`, and `urlencoding` are already available.

## Testing Strategy

Default tests remain offline, using `wiremock` to mock the S3 endpoint.

Unit tests:

- parse `ImageUploadConfig` with `provider="ipfs3"` and required-field validation (missing `gateway_url`, missing `bucket`, invalid `endpoint_url`, invalid `gateway_url`).
- preserve `pixi` as the default provider.
- preserve existing `s3`/`catbox` parsing and validation unchanged.

Wiremock tests:

- configure `IpfS3Uploader` with a wiremock endpoint,
- assert `PUT` requests land at the expected path-style bucket/key path,
- assert the request body bytes match the input image,
- mock the response with a 200 status and an `etag` header holding a CID (e.g. `"bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi"`),
- assert returned URLs are `{gateway_url}/{CID}` (no quotes, no object key),
- assert a 200 response with a missing `etag` header returns an upload error,
- assert a non-2xx response returns an upload error.

Existing tests:

- All existing `S3Uploader`, `CatboxUploader`, `PixiUploader`, and `TelegraphClient` tests must continue to pass without modification.

## Migration and Compatibility

No database migration is needed. No existing config is affected: `provider` defaults to `pixi`, and `[image_upload.ipfs3]` is optional. Operators who do not configure ipfS3 see no behavior change.

`config.toml.example` is updated to document the new `ipfs3` provider and its configuration block, including a note that `gateway_url` is independent of `endpoint_url` and that only plaintext (non-encrypted) uploads are gateway-accessible.

## Acceptance Criteria

- `config.toml.example` documents the `ipfs3` provider alongside `pixi`, `s3`, and `catbox`.
- `Config` can load `image_upload.provider="ipfs3"` with endpoint/bucket/region/credentials/gateway_url and optional key_prefix/path_style.
- `ImageUploadProvider` has an `IpfS3` variant; `ImageUploadConfig` has an `ipfs3: Option<IpfS3UploaderConfig>` field; `build_uploader()` dispatches to `IpfS3Uploader`.
- `IpfS3Uploader` uploads image bytes with detected content type through `PutObject`, reads the CID from the `ETag` response header, strips quotes, and returns `{gateway_url}/{CID}` URLs.
- Missing or empty `ETag` on a 2xx response returns an upload error.
- Non-2xx `PutObject` response returns an upload error.
- Existing `pixi` default behavior, `S3Uploader`, `CatboxUploader`, and all existing tests are unchanged.
- `make ci` passes: `fmt-check -> clippy -Dwarnings -> check -> test -> release build`.
