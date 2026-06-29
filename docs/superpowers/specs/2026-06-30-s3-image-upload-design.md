# S3-Compatible Image Upload Design

## Context

EH Telegraph publishing currently uploads images through the hard-coded pixi.mg endpoint inside `eh_client::telegraph::TelegraphClient`, then creates Telegraph pages from the returned image URLs. pixi.mg is anonymous but can rate-limit and is not operator-controlled. The requested change is to add S3-compatible image upload configuration so the bot can upload images to an operator-owned object store and place public object URLs in Telegraph pages.

The selected URL model is explicit public URL generation: each uploaded object returns `public_base_url + "/" + key`. The bucket or CDN behind `public_base_url` must be publicly readable. Presigned URLs are intentionally out of scope because expiring URLs would make Telegraph pages decay over time.

## Goals

1. Allow operators to choose between the existing pixi.mg uploader and a new S3-compatible uploader.
2. Use S3-compatible `PutObject` with configurable endpoint, bucket, region, credentials, key prefix, path-style mode, and public base URL.
3. Reuse the same uploader abstraction from the EH upload worker and the manual live EH example so behavior is consistent.
4. Preserve existing pixi.mg behavior as the default when no S3 configuration is present.
5. Avoid logging S3 credentials or embedding secrets in generated URLs.
6. Keep default tests offline and deterministic.

## Non-Goals

- Do not implement presigned URL generation.
- Do not add S3 bucket creation, ACL management, or CDN provisioning.
- Do not make any real S3 network calls in CI.
- Do not change Telegraph page creation semantics beyond replacing the source of image URLs.
- Do not change EH archive download or queue state-machine behavior.

## Configuration

Add a new top-level image upload configuration, independent of EH site/cookie settings:

```toml
[image_upload]
# Default: "pixi". Set to "s3" to upload images to an S3-compatible object store.
provider = "pixi"

[image_upload.s3]
endpoint_url = "https://<account>.r2.cloudflarestorage.com"
bucket = "pixivbot-images"
region = "auto"
access_key_id = "..."
secret_access_key = "..."
public_base_url = "https://cdn.example.com/pixivbot-images"
key_prefix = "eh"
path_style = true
```

Rules:

- `provider = "pixi"` uses the current pixi.mg uploader and ignores `[image_upload.s3]`.
- `provider = "s3"` requires every S3 field except `key_prefix`, which defaults to empty, and `path_style`, which defaults to `true` for compatibility with MinIO/R2-style endpoints.
- `public_base_url` is trimmed of trailing slashes before URL generation.
- Generated object keys are relative paths such as `eh/20260630123045-0001-a1b2c3d4.jpg`; generated public URLs are `public_base_url/key`.
- Secrets may also be overridden through the existing config environment mechanism, e.g. `PIX__IMAGE_UPLOAD__S3__SECRET_ACCESS_KEY`.

## Architecture

Introduce an uploader boundary inside `eh_client`:

```rust
#[async_trait]
pub trait ImageUploader: Send + Sync {
    async fn upload_images(&self, images: &[ImageUploadInput<'_>]) -> Result<Vec<String>>;
}

pub struct ImageUploadInput<'a> {
    pub filename: &'a str,
    pub bytes: &'a [u8],
}
```

Concrete implementations:

- `PixiUploader`: wraps current pixi.mg upload behavior, including max batch size 5 and existing 429 exponential backoff.
- `S3Uploader`: uploads each image with `PutObject`, sets the detected content type, and returns deterministic public URLs.

`TelegraphClient` remains responsible for `createPage` / `create_gallery_page`. It should no longer own the only image upload path. EH upload worker orchestration becomes:

1. Extract image entries from ZIP.
2. Skip entries larger than the existing 6 MiB limit.
3. Pass image bytes and filenames to configured `ImageUploader`.
4. Create Telegraph page with returned URLs.
5. Persist `telegraph_url` as today.

`src/main.rs` constructs the uploader from `Config.image_upload` and passes it into `EhUploadWorker`. The live regression example uses the same construction helper so manual S3 testing matches production.

## S3 Key and URL Generation

S3 object keys must avoid collisions and preserve useful extensions. The uploader will generate keys as:

```text
{key_prefix}/{yyyyMMddHHmmss}-{sequence}-{hash8}.{ext}
```

Where:

- `key_prefix` is omitted if empty.
- `sequence` is the image index in the upload batch or gallery extraction order.
- `hash8` is a short content hash to reduce accidental collision risk.
- `ext` comes from detected content type or the original filename extension when safe.

The returned URL is built by percent-encoding each key path segment and joining it to `public_base_url`. The secret key and bucket endpoint never appear in returned URLs.

## Error Handling

- Config validation fails startup if `provider="s3"` and any required S3 field is missing.
- S3 upload failure returns an error to `EhUploadWorker`; existing retry/fallback logic handles transient and permanent failures.
- If S3 succeeds for some images and then fails, the worker treats the entire Telegraph upload attempt as failed. The design does not attempt object cleanup in this first version because object deletion would require another permission and can be handled by lifecycle policies.
- If returned public URLs are empty or malformed, `create_gallery_page` is not called.
- Logs may include bucket name, endpoint host, and object key, but must not include access key ID or secret access key.

## Dependencies

Use AWS SDK for Rust crates:

- `aws-config`
- `aws-credential-types`
- `aws-sdk-s3`
- `aws-smithy-runtime-api` or SDK config types only if required by the concrete client builder

The S3 client must support:

- custom `endpoint_url`,
- explicit credentials,
- explicit region,
- path-style addressing for S3-compatible providers.

## Testing Strategy

Default tests remain offline.

Unit tests:

- parse `ImageUploadConfig` defaults and S3 required-field validation,
- generate S3 keys with prefix and safe extension,
- generate public URLs with trailing-slash normalization and segment encoding,
- preserve pixi.mg as default provider.

Wiremock tests:

- configure `S3Uploader` with a wiremock endpoint,
- assert `PUT` requests land at the expected path-style bucket/key path,
- assert the body bytes match the input image,
- assert returned URLs use `public_base_url`, not the S3 endpoint,
- assert failed `PUT` returns an upload error.

Integration tests:

- EH upload worker test using a fake uploader verifies the worker is uploader-agnostic.
- Existing pixi.mg/Telegraph tests continue to pass.

Manual live test:

- The live EH example can be run with `image_upload.provider=s3` style environment overrides once implementation is complete.
- No real S3 credentials are committed or required for CI.

## Migration and Compatibility

No database migration is needed. Existing queued EH entries keep the same lifecycle and state fields. Operators who do not configure `[image_upload]` get current pixi.mg behavior.

## Acceptance Criteria

- `config.toml.example` documents pixi default and S3-compatible configuration.
- `Config` can load `image_upload.provider="s3"` with endpoint/bucket/region/credentials/public base URL.
- EH upload worker uses the configured uploader for Telegraph image URLs.
- S3 uploader uploads image bytes with content type and returns `public_base_url + key` URLs.
- Pixi uploader remains default and existing tests pass.
- S3 errors flow into existing retry/fallback behavior without panics or credential leaks.
- Default CI/test commands do not require S3 credentials or network access.
