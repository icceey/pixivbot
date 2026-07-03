# ipfS3 Image Upload Provider Implementation Plan

> **For agentic workers:** Use the subagent-driven-development skill to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `ipfs3` as a fourth `ImageUploadProvider` that uploads images through the S3-compatible ipfS3 gateway and derives public URLs from the response `ETag` (IPFS CID).

**Architecture:** A new `IpfS3Uploader` mirrors the existing `S3Uploader` transport (same `rust-s3` `Bucket::put_object_with_content_type` call, same object-key shape) but, instead of building the public URL from `public_base_url + key`, it reads the IPFS CID from the `ETag` response header and returns `{gateway_url}/{CID}`. A new `IpfS3UploaderConfig` holds endpoint/bucket/region/credentials/gateway_url/key_prefix/path_style and is validated with the existing `required_config` + `validate_http_url` helpers. No new dependencies.

**Tech Stack:** Rust 1.94, `rust-s3` 0.37 (already in `eh_client/Cargo.toml`), `wiremock` 0.6 (dev), `async-trait`, `chrono`, `urlencoding`.

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `eh_client/src/telegraph.rs` | Image uploader abstraction + all provider implementations | Add `IpfS3UploaderConfig`, `ResolvedIpfS3UploaderConfig`, `IpfS3Uploader`; extend `ImageUploadProvider`, `ImageUploadConfig`, `build_uploader()`; add tests |
| `eh_client/src/lib.rs` | Crate-level re-exports | Export `IpfS3Uploader`, `IpfS3UploaderConfig` |
| `config.toml.example` | Public config reference | Document `[image_upload.ipfs3]` block |

All three files already exist and follow established patterns. No new files are created.

---

## Task 1: Add `IpfS3UploaderConfig` and config validation

**Files:**
- Modify: `eh_client/src/telegraph.rs` (after `S3UploaderConfig` impl block, ~L236; and `ImageUploadConfig` struct, ~L143-165; and the `tests` module)

This task adds the config types and the `ipfs3` field on `ImageUploadConfig`, plus unit tests for validation. It does **not** add the `IpfS3` enum variant or touch `build_uploader()` — that comes in Task 2 — so the crate stays compilable throughout.

- [ ] **Step 1: Write the failing config validation tests**

Add these tests inside the `#[cfg(test)] mod tests` block in `eh_client/src/telegraph.rs`, after the existing `s3_config_rejects_unsafe_endpoint_url` test (which ends around L903):

```rust
    fn complete_ipfs3_config(endpoint: &str, gateway_url: &str) -> IpfS3UploaderConfig {
        IpfS3UploaderConfig {
            endpoint_url: Some(endpoint.to_string()),
            bucket: Some("bucket".to_string()),
            region: Some("us-east-1".to_string()),
            access_key_id: Some("key".to_string()),
            secret_access_key: Some("secret".to_string()),
            gateway_url: Some(gateway_url.to_string()),
            key_prefix: "eh".to_string(),
            path_style: true,
        }
    }

    #[test]
    fn ipfs3_config_requires_fields_for_provider() {
        let cfg = IpfS3UploaderConfig::default();
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("image_upload.ipfs3.endpoint_url"));
    }

    #[test]
    fn ipfs3_config_rejects_invalid_gateway_url() {
        let mut cfg = complete_ipfs3_config("http://localhost:9000", "not a url");
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("image_upload.ipfs3.gateway_url"));

        cfg.gateway_url = Some("ftp://ipfs.io/ipfs".to_string());
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("must use http or https"));
    }

    #[test]
    fn ipfs3_config_rejects_gateway_url_with_secret_or_non_path_parts() {
        let mut cfg = complete_ipfs3_config(
            "http://localhost:9000",
            "https://user:pass@ipfs.io/ipfs",
        );
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("must not contain userinfo"));

        cfg.gateway_url = Some("https://ipfs.io/ipfs?token=secret".to_string());
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("must not contain query"));

        cfg.gateway_url = Some("https://ipfs.io/ipfs#frag".to_string());
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("must not contain fragment"));
    }

    #[test]
    fn ipfs3_config_rejects_unsafe_endpoint_url() {
        let mut cfg = complete_ipfs3_config("ftp://localhost:9000", "https://ipfs.io/ipfs");
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("must use http or https"));

        cfg.endpoint_url = Some("https://user:secret@ipfs3.example.com".to_string());
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("must not contain userinfo"));

        cfg.endpoint_url = Some("https://ipfs3.example.com?token=secret".to_string());
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("must not contain query"));
    }

    #[test]
    fn ipfs3_config_trims_gateway_url_trailing_slash() {
        let cfg = complete_ipfs3_config("http://localhost:9000", "https://ipfs.io/ipfs/");
        let resolved = cfg.required().unwrap();
        assert_eq!(resolved.gateway_url, "https://ipfs.io/ipfs");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p eh_client --lib telegraph::tests::ipfs3_config`
Expected: COMPILE ERROR — `IpfS3UploaderConfig` not found.

- [ ] **Step 3: Implement `IpfS3UploaderConfig`, `ResolvedIpfS3UploaderConfig`, and `required()`**

Add the following block in `eh_client/src/telegraph.rs`, immediately **after** the `S3UploaderConfig` impl block (after L236, before the `required_config` helper at L238). This mirrors the S3 config pattern exactly:

```rust
#[derive(Debug, Clone, Deserialize, Default)]
pub struct IpfS3UploaderConfig {
    #[serde(default)]
    pub endpoint_url: Option<String>,
    #[serde(default)]
    pub bucket: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub access_key_id: Option<String>,
    #[serde(default)]
    pub secret_access_key: Option<String>,
    #[serde(default)]
    pub gateway_url: Option<String>,
    #[serde(default)]
    pub key_prefix: String,
    #[serde(default = "default_s3_path_style")]
    pub path_style: bool,
}

impl IpfS3UploaderConfig {
    fn required(&self) -> Result<ResolvedIpfS3UploaderConfig> {
        Ok(ResolvedIpfS3UploaderConfig {
            endpoint_url: validate_http_url(
                "image_upload.ipfs3.endpoint_url",
                &required_config("image_upload.ipfs3.endpoint_url", &self.endpoint_url)?,
            )?,
            bucket: required_config("image_upload.ipfs3.bucket", &self.bucket)?,
            region: required_config("image_upload.ipfs3.region", &self.region)?,
            access_key_id: required_config(
                "image_upload.ipfs3.access_key_id",
                &self.access_key_id,
            )?,
            secret_access_key: required_config(
                "image_upload.ipfs3.secret_access_key",
                &self.secret_access_key,
            )?,
            gateway_url: validate_http_url(
                "image_upload.ipfs3.gateway_url",
                &required_config("image_upload.ipfs3.gateway_url", &self.gateway_url)?,
            )?
            .trim_end_matches('/')
            .to_string(),
            key_prefix: self.key_prefix.trim_matches('/').to_string(),
            path_style: self.path_style,
        })
    }
}

#[derive(Debug, Clone)]
struct ResolvedIpfS3UploaderConfig {
    endpoint_url: String,
    bucket: String,
    region: String,
    access_key_id: String,
    secret_access_key: String,
    gateway_url: String,
    key_prefix: String,
    path_style: bool,
}
```

Then add the `ipfs3` field to `ImageUploadConfig` (at ~L143-151). Change:

```rust
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ImageUploadConfig {
    #[serde(default)]
    pub provider: ImageUploadProvider,
    #[serde(default)]
    pub s3: Option<S3UploaderConfig>,
    #[serde(default)]
    pub catbox: CatboxUploaderConfig,
}
```

to:

```rust
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ImageUploadConfig {
    #[serde(default)]
    pub provider: ImageUploadProvider,
    #[serde(default)]
    pub s3: Option<S3UploaderConfig>,
    #[serde(default)]
    pub ipfs3: Option<IpfS3UploaderConfig>,
    #[serde(default)]
    pub catbox: CatboxUploaderConfig,
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p eh_client --lib telegraph::tests::ipfs3_config`
Expected: PASS — all 5 new `ipfs3_config_*` tests pass.

Also run the full eh_client test suite to confirm no regression:
Run: `cargo test -p eh_client`
Expected: PASS — all existing tests still pass.

- [ ] **Step 5: Run clippy and fmt**

Run: `cargo fmt -p eh_client && cargo clippy -p eh_client -- -Dwarnings`
Expected: no warnings, exit code 0.

- [ ] **Step 6: Commit**

```powershell
git add eh_client/src/telegraph.rs
git commit -m "feat(eh_client): add IpfS3UploaderConfig and config validation"
```

---

## Task 2: Add `IpfS3Uploader` with CID-based URL generation

**Files:**
- Modify: `eh_client/src/telegraph.rs` (add `IpfS3Uploader` struct + impl after `S3Uploader` impl, ~L547; add `IpfS3` variant to `ImageUploadProvider`; wire `build_uploader()`)

This task implements the uploader that reads the IPFS CID from the `ETag` response header and returns `{gateway_url}/{CID}`. It also adds the `IpfS3` enum variant and dispatches it in `build_uploader()`.

- [ ] **Step 1: Write the failing wiremock success test**

Add this test inside the `tests` module, after the `complete_ipfs3_config` helper added in Task 1:

```rust
    #[tokio::test]
    async fn ipfs3_uploader_puts_object_and_returns_gateway_url_from_etag() {
        use wiremock::matchers::{body_bytes, header, method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let cid = "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi";
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/bucket/eh/\d{14}-0001-[0-9a-f]{8}\.png$"))
            .and(body_bytes(vec![
                0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'n',
            ]))
            .respond_with(
                ResponseTemplate::new(200).insert_header("etag", format!("\"{cid}\"")),
            )
            .expect(1)
            .mount(&server)
            .await;

        let uploader = IpfS3Uploader::from_config(&complete_ipfs3_config(
            &server.uri(),
            "https://ipfs.io/ipfs",
        ))
        .unwrap();
        let urls = uploader
            .upload_images(&[ImageUploadInput {
                filename: "image.png",
                bytes: b"\x89PNG\r\n\x1a\n",
            }])
            .await
            .unwrap();

        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0], format!("https://ipfs.io/ipfs/{cid}"));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p eh_client --lib telegraph::tests::ipfs3_uploader_puts_object`
Expected: COMPILE ERROR — `IpfS3Uploader` not found.

- [ ] **Step 3: Implement `IpfS3Uploader` struct, `from_config`, `object_key`, and `upload_images`**

Add the following block in `eh_client/src/telegraph.rs`, immediately **after** the `S3Uploader` impl block (after L547, before the `extension_for_content_type` helper at L549):

```rust
pub struct IpfS3Uploader {
    bucket: Box<Bucket>,
    config: ResolvedIpfS3UploaderConfig,
}

impl IpfS3Uploader {
    pub fn from_config(config: &IpfS3UploaderConfig) -> Result<Self> {
        let config = config.required()?;
        let credentials = Credentials::new(
            Some(&config.access_key_id),
            Some(&config.secret_access_key),
            None,
            None,
            None,
        )
        .map_err(|e| Error::Other(format!("invalid ipfS3 credentials: {e}")))?;
        let region = Region::Custom {
            region: config.region.clone(),
            endpoint: config.endpoint_url.clone(),
        };
        let mut bucket = Bucket::new(&config.bucket, region, credentials)
            .map_err(|e| Error::Other(format!("failed to build ipfS3 bucket client: {e}")))?;
        if config.path_style {
            bucket = bucket.with_path_style();
        }
        Ok(Self { bucket, config })
    }

    fn object_key(&self, index: usize, input: &ImageUploadInput<'_>) -> String {
        let timestamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
        let hash = short_hash_hex(input.bytes);
        let ext = extension_for_upload(input.filename, input.bytes);
        let filename = format!("{timestamp}-{index:04}-{hash}.{ext}");
        if self.config.key_prefix.is_empty() {
            filename
        } else {
            format!("{}/{}", self.config.key_prefix, filename)
        }
    }
}

#[async_trait]
impl ImageUploader for IpfS3Uploader {
    async fn upload_images(&self, images: &[ImageUploadInput<'_>]) -> Result<Vec<String>> {
        let mut urls = Vec::with_capacity(images.len());
        for (index, image) in images.iter().enumerate() {
            let key = self.object_key(index + 1, image);
            let content_type = detect_content_type(image.bytes);
            let response = self
                .bucket
                .put_object_with_content_type(&key, image.bytes, &content_type)
                .await
                .map_err(|e| {
                    Error::Other(format!("ipfS3 put_object failed for key {key}: {e}"))
                })?;
            let status = response.status_code();
            if !(200..300).contains(&status) {
                return Err(Error::Api {
                    message: format!("ipfS3 put_object returned {status} for key {key}"),
                    status,
                });
            }
            let etag = response
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(|v| v.trim_matches('"'))
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    Error::Other(format!(
                        "ipfS3 put_object for key {key} returned no ETag (CID); \
                         cannot build public URL"
                    ))
                })?;
            urls.push(format!("{}/{etag}", self.config.gateway_url));
        }
        Ok(urls)
    }
}
```

- [ ] **Step 4: Add the `IpfS3` variant to `ImageUploadProvider`**

Change the enum (at ~L134-141) from:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ImageUploadProvider {
    #[default]
    Pixi,
    S3,
    Catbox,
}
```

to:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ImageUploadProvider {
    #[default]
    Pixi,
    S3,
    Catbox,
    IpfS3,
}
```

- [ ] **Step 5: Wire `build_uploader()` dispatch**

Change the `build_uploader` match (at ~L155-164) from:

```rust
        match self.provider {
            ImageUploadProvider::Pixi => Ok(Arc::new(PixiUploader::new())),
            ImageUploadProvider::S3 => Ok(Arc::new(S3Uploader::from_config(
                self.s3.as_ref().ok_or_else(|| {
                    Error::Other("image_upload.s3 is required when provider=s3".into())
                })?,
            )?)),
            ImageUploadProvider::Catbox => Ok(Arc::new(CatboxUploader::from_config(&self.catbox)?)),
        }
```

to:

```rust
        match self.provider {
            ImageUploadProvider::Pixi => Ok(Arc::new(PixiUploader::new())),
            ImageUploadProvider::S3 => Ok(Arc::new(S3Uploader::from_config(
                self.s3.as_ref().ok_or_else(|| {
                    Error::Other("image_upload.s3 is required when provider=s3".into())
                })?,
            )?)),
            ImageUploadProvider::IpfS3 => Ok(Arc::new(IpfS3Uploader::from_config(
                self.ipfs3.as_ref().ok_or_else(|| {
                    Error::Other("image_upload.ipfs3 is required when provider=ipfs3".into())
                })?,
            )?)),
            ImageUploadProvider::Catbox => Ok(Arc::new(CatboxUploader::from_config(&self.catbox)?)),
        }
```

- [ ] **Step 6: Run the success test to verify it passes**

Run: `cargo test -p eh_client --lib telegraph::tests::ipfs3_uploader_puts_object`
Expected: PASS — URL equals `https://ipfs.io/ipfs/{cid}`.

- [ ] **Step 7: Write the missing-ETag error test**

Add this test after the success test:

```rust
    #[tokio::test]
    async fn ipfs3_uploader_returns_error_when_etag_missing() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/bucket/eh/.*\.png$"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let uploader = IpfS3Uploader::from_config(&complete_ipfs3_config(
            &server.uri(),
            "https://ipfs.io/ipfs",
        ))
        .unwrap();
        let err = uploader
            .upload_images(&[ImageUploadInput {
                filename: "image.png",
                bytes: b"\x89PNG\r\n\x1a\n",
            }])
            .await
            .unwrap_err();

        assert!(err.to_string().contains("no ETag (CID)"));
    }
```

- [ ] **Step 8: Run the missing-ETag test**

Run: `cargo test -p eh_client --lib telegraph::tests::ipfs3_uploader_returns_error_when_etag_missing`
Expected: PASS.

- [ ] **Step 9: Write the non-2xx error test**

Add this test after the missing-ETag test:

```rust
    #[tokio::test]
    async fn ipfs3_uploader_returns_error_on_failed_put() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/bucket/eh/.*\.jpg$"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let uploader = IpfS3Uploader::from_config(&complete_ipfs3_config(
            &server.uri(),
            "https://ipfs.io/ipfs",
        ))
        .unwrap();
        let err = uploader
            .upload_images(&[ImageUploadInput {
                filename: "image.jpg",
                bytes: b"\xFF\xD8\xFF\x00",
            }])
            .await
            .unwrap_err();

        assert!(err.to_string().contains("ipfS3 put_object returned 500"));
    }
```

- [ ] **Step 10: Run the non-2xx test**

Run: `cargo test -p eh_client --lib telegraph::tests::ipfs3_uploader_returns_error_on_failed_put`
Expected: PASS.

- [ ] **Step 11: Run the full test suite + clippy + fmt**

Run: `cargo test -p eh_client && cargo fmt -p eh_client && cargo clippy -p eh_client -- -Dwarnings`
Expected: all tests pass, no clippy warnings, exit code 0.

- [ ] **Step 12: Commit**

```powershell
git add eh_client/src/telegraph.rs
git commit -m "feat(eh_client): add IpfS3Uploader with CID-based URL generation"
```

---

## Task 3: Export new types from `eh_client`

**Files:**
- Modify: `eh_client/src/lib.rs` (L10-12)

- [ ] **Step 1: Update the re-export list**

Change `eh_client/src/lib.rs` L10-12 from:

```rust
pub use telegraph::{
    CatboxUploader, CatboxUploaderConfig, ImageUploadConfig, ImageUploadInput, ImageUploadProvider,
    ImageUploader, PixiUploader, S3Uploader, S3UploaderConfig, TelegraphClient,
};
```

to:

```rust
pub use telegraph::{
    CatboxUploader, CatboxUploaderConfig, ImageUploadConfig, ImageUploadInput, ImageUploadProvider,
    ImageUploader, IpfS3Uploader, IpfS3UploaderConfig, PixiUploader, S3Uploader,
    S3UploaderConfig, TelegraphClient,
};
```

- [ ] **Step 2: Verify the crate compiles**

Run: `cargo check -p eh_client`
Expected: exit code 0, no errors.

- [ ] **Step 3: Commit**

```powershell
git add eh_client/src/lib.rs
git commit -m "feat(eh_client): export IpfS3Uploader and IpfS3UploaderConfig"
```

---

## Task 4: Document the `ipfs3` provider in `config.toml.example`

**Files:**
- Modify: `config.toml.example` (L84-106 region)

- [ ] **Step 1: Add the `ipfs3` documentation block**

In `config.toml.example`, update the header comment block (L84-88) and add the new block. Change:

```toml
# ----------------------------------------------------------------------------
# Image upload backend for Telegraph pages. Default provider is "pixi". Set
# provider to "s3" to upload images to an S3-compatible object store, or
# "catbox" to use Catbox.
# ----------------------------------------------------------------------------
# [image_upload]
# provider = "pixi"
#
# [image_upload.s3]
# endpoint_url = "https://<account>.r2.cloudflarestorage.com"
# bucket = "pixivbot-images"
# region = "auto"
# access_key_id = "your_access_key_id"
# secret_access_key = "your_secret_access_key"
# public_base_url = "https://cdn.example.com/pixivbot-images"
# key_prefix = "eh"
# path_style = true
#
# [image_upload.catbox]
# # Catbox uploads are public; userhash is optional. Anonymous uploads cannot be
# # deleted through the Catbox API later.
# api_url = "https://catbox.moe/user/api.php"
# # userhash = "your_catbox_userhash"
```

to:

```toml
# ----------------------------------------------------------------------------
# Image upload backend for Telegraph pages. Default provider is "pixi". Set
# provider to "s3" to upload images to an S3-compatible object store, "catbox"
# to use Catbox, or "ipfs3" to use an ipfS3 (S3-compatible IPFS gateway) store.
# ----------------------------------------------------------------------------
# [image_upload]
# provider = "pixi"
#
# [image_upload.s3]
# endpoint_url = "https://<account>.r2.cloudflarestorage.com"
# bucket = "pixivbot-images"
# region = "auto"
# access_key_id = "your_access_key_id"
# secret_access_key = "your_secret_access_key"
# public_base_url = "https://cdn.example.com/pixivbot-images"
# key_prefix = "eh"
# path_style = true
#
# [image_upload.ipfs3]
# # ipfS3 is an S3-compatible gateway in front of IPFS (Kubo). It returns the
# # IPFS Content ID (CID) of each uploaded object in the ETag response header.
# # The public URL is built as {gateway_url}/{CID}, so reads are served by any
# # public IPFS gateway, independent of endpoint_url. Only plaintext (non-
# # encrypted) uploads are gateway-accessible; do not enable SSE-S3/SSE-C.
# # See: https://github.com/hugefiver/ipfS3
# endpoint_url = "http://localhost:9000"
# bucket = "pixivbot-images"
# region = "us-east-1"
# access_key_id = "test"
# secret_access_key = "test"
# gateway_url = "https://ipfs.io/ipfs"
# key_prefix = "eh"
# path_style = true
#
# [image_upload.catbox]
# # Catbox uploads are public; userhash is optional. Anonymous uploads cannot be
# # deleted through the Catbox API later.
# api_url = "https://catbox.moe/user/api.php"
# # userhash = "your_catbox_userhash"
```

- [ ] **Step 2: Verify the example still parses as valid TOML**

Run: `cargo check -p pixivbot`
Expected: exit code 0 (the example is not compiled, but the crate must still build since `config.toml.example` is only documentation).

Note: `config.toml.example` is not loaded at build time; this is a docs-only change. A git diff check suffices:
Run: `git diff --check -- config.toml.example`
Expected: no whitespace errors.

- [ ] **Step 3: Commit**

```powershell
git add config.toml.example
git commit -m "docs: document ipfs3 image upload provider in config.toml.example"
```

---

## Task 5: Run full CI verification

**Files:** none (verification only)

- [ ] **Step 1: Run `make ci`**

Run: `make ci`
Expected: `fmt-check -> clippy (RUSTFLAGS=-Dwarnings) -> check -> test -> build` all pass, exit code 0.

- [ ] **Step 2: Confirm no regressions in existing tests**

Inspect the `make ci` output: all `eh_client` tests (S3, Catbox, Pixi, Telegraph, ipfs3) and all `pixivbot` tests must pass. If any pre-existing test fails, investigate whether it is related to this change (it should not be — the change is additive).

- [ ] **Step 3: Final status report**

Report: list the commits made, the test counts, and confirm `make ci` exit code 0. No commit needed for this step.
