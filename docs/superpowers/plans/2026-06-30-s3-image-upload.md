# Configurable Image Upload Implementation Plan

> **For agentic workers:** This session is explicitly not using subagents and must not commit code changes. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add configurable S3-compatible and Catbox image upload for EH Telegraph publishing, while keeping pixi.mg as the default provider.

**Architecture:** Introduce an `ImageUploader` boundary in `eh_client`, with `PixiUploader` wrapping the current default pixi.mg behavior, `CatboxUploader` as an explicit option, and `S3Uploader` using S3-compatible `PutObject`. Root config selects the provider and `EhUploadWorker` uploads ZIP images through the configured uploader before creating Telegraph pages.

**Tech Stack:** Rust 1.94, `eh_client`, `rust-s3`, `reqwest`, `tokio`, `wiremock`, existing `config` env override mechanism.

---

### Task 1: Uploader Abstraction and Config Types

**Files:**
- Modify: `eh_client/Cargo.toml`
- Modify: `eh_client/src/telegraph.rs`
- Modify: `eh_client/src/lib.rs`

- [ ] Add dependencies: `async-trait`, `rust-s3`, and `chrono` to `eh_client/Cargo.toml`.
- [ ] Add `ImageUploadProvider`, `ImageUploadConfig`, `S3UploaderConfig`, `CatboxUploaderConfig`, `ImageUploadInput`, and `ImageUploader` in `eh_client/src/telegraph.rs`.
- [ ] Implement `PixiUploader` by moving the current pixi.mg batch upload behavior behind `ImageUploader` without changing the public `TelegraphClient` page APIs.
- [ ] Export the new types from `eh_client/src/lib.rs`.
- [ ] Add unit tests for pixi default config and required S3 validation.

### Task 2: S3 Uploader

**Files:**
- Modify: `eh_client/src/telegraph.rs`

- [ ] Implement `S3Uploader::from_config` using explicit credentials, region, endpoint URL, and path-style setting.
- [ ] Implement object key generation as `{key_prefix}/{yyyyMMddHHmmss}-{sequence}-{hash8}.{ext}`.
- [ ] Implement public URL generation by trimming `public_base_url`, percent-encoding each key path segment, and joining with `/`.
- [ ] Implement `ImageUploader` for `S3Uploader` using `put_object_with_content_type` with detected content type.
- [ ] Add unit tests for key generation, public URL generation, extension/content type behavior, and required config errors.
- [ ] Add a wiremock test that verifies `PUT /bucket/key`, request body, and returned public URL.

### Task 3: Application Config and Wiring

**Files:**
- Modify: `src/config.rs`
- Modify: `config.toml.example`
- Modify: `src/main.rs`

- [ ] Add `image_upload: eh_client::ImageUploadConfig` to root `Config` with default pixi provider.
- [ ] Add config tests for default pixi and S3 validation-compatible construction.
- [ ] Document `[image_upload]` and `[image_upload.s3]` in `config.toml.example` without real secrets.
- [ ] Document `[image_upload.catbox]` in `config.toml.example` with optional `userhash` and default Catbox API URL.
- [ ] Build the configured uploader in `src/main.rs` before starting `EhUploadWorker`; fail startup with a clear error if S3 config is invalid.

### Task 3.5: Catbox Uploader

**Files:**
- Modify: `eh_client/src/telegraph.rs`
- Modify: `config.toml.example`

- [ ] Implement `CatboxUploader::from_config` with default API URL `https://catbox.moe/user/api.php` and optional `userhash`.
- [ ] Implement `ImageUploader` for `CatboxUploader` with multipart fields `reqtype=fileupload`, optional `userhash`, and `fileToUpload`.
- [ ] Treat non-2xx or non-URL response bodies as upload errors.
- [ ] Add wiremock tests for successful Catbox URL response and non-URL error response.

### Task 4: EH Upload Worker Integration

**Files:**
- Modify: `src/scheduler/eh_engine.rs`

- [ ] Add `image_uploader: Arc<dyn eh_client::ImageUploader>` to `EhUploadWorker`.
- [ ] Update `EhUploadWorker::new` call sites and tests.
- [ ] Replace `telegraph.upload_images_with_retry` calls with `image_uploader.upload_images` using extracted filenames and bytes.
- [ ] Keep `TelegraphClient::create_gallery_page` unchanged for page creation.
- [ ] Update existing EH upload worker tests to inject a configured uploader instead of relying on `TelegraphClient` image upload methods.

### Task 5: Verification

**Files:**
- No planned production file edits beyond Tasks 1-4.

- [ ] Run `cargo fmt`.
- [ ] Run `cargo test -p eh_client`.
- [ ] Run `cargo test -p pixivbot --no-default-features eh_engine`.
- [ ] Run `cargo test -p pixivbot --no-default-features config`.
- [ ] Run `cargo clippy -p eh_client --tests -- -D warnings`.
- [ ] Run `cargo clippy -p pixivbot --no-default-features --tests -- -D warnings`.
- [ ] Run `git diff --check`.
- [ ] Confirm `git status --short` shows only intended uncommitted changes and no commits were created.
