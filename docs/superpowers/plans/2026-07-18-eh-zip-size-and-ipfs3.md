# EH ZIP Size Limit and ipfS3 ZIP Upload Implementation Plan

> **For agentic workers:** Use the subagent-driven-development skill to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a configurable EH archive size gate before paid archive requests, and let configured ipfS3 uploads send EH ZIP archives once instead of uploading each image separately.

**Architecture:** Keep the EH size gate in scheduler/download code, before `prepare_archive_download()` and `download_archive_with_request()`. Keep provider-specific ZIP extraction inside `eh_client::telegraph::IpfS3Uploader` behind an optional `ImageUploader` capability, so non-ipfS3 uploaders and unconfigured ipfS3 deployments keep the current per-image path.

**Tech Stack:** Rust 1.94, anyhow, async-trait, SeaORM repo/entities, wiremock tests, zip crate, rust-s3, reqwest, teloxide MarkdownV2.

**Global Constraints:**
- Do not execute `git commit`, `git push`, `git tag`, or other git write commands in this repository.
- Do not read local `/config.toml`; update only `config.toml.example` for public config docs.
- Size gate must run before any EH archive `archiver.php` POST; checking during response/download is too late.
- Do not hard-code undocumented ipfS3 ZIP-extract protocol details beyond the explicit documented assumption that a configured deployment makes ZIP entries addressable under the returned root CID.
- Preserve existing behavior when `ehentai.max_archive_size_mb = 0` or `image_upload.ipfs3.zip_extract_enabled = false`.

---

## File map

- Modify `src/config.rs`: add `EhentaiConfig::max_archive_size_mb`, default/helper/tests.
- Modify `config.toml.example`: document EH size limit and ipfS3 ZIP extraction flag.
- Modify `src/scheduler/eh_engine.rs`: add shared metadata size guard, call it in foreground/background logged-in archive paths, add ordered ZIP entry-name collection, and prefer ZIP upload capability before per-image extraction.
- Modify `eh_client/src/telegraph.rs`: add `zip_extract_enabled`, resolved config field, ZIP archive upload input, trait method, ipfS3 implementation, URL/path helpers, and tests.
- Modify `eh_client/src/lib.rs`: re-export `ZipArchiveUploadInput` so scheduler/tests can use `eh_client::ZipArchiveUploadInput`.
- Test existing files: `src/config.rs`, `eh_client/src/telegraph.rs`, `src/scheduler/eh_engine.rs`.

## Task 1: Config and EH size gate

**Files:**
- Modify: `src/config.rs`
- Modify: `config.toml.example`
- Modify: `src/scheduler/eh_engine.rs`

**Interfaces:**
- Consumes: existing `EhClient::get_metadata(&[(u64, &str)]) -> Result<Vec<EhGallery>>`, `EhentaiConfig::download_rate_limit_bytes()`.
- Produces: `EhentaiConfig::max_archive_size_bytes(&self) -> Option<u64>` and `ensure_eh_archive_under_size_limit(client, config, gid, token) -> Result<()>`.

- [ ] **Step 1: Add failing config tests**

Add to `src/config.rs` tests:

```rust
#[test]
fn test_eh_max_archive_size_defaults_to_300_mib() {
    let cfg = EhentaiConfig::default();
    assert_eq!(cfg.max_archive_size_mb, 300);
    assert_eq!(cfg.max_archive_size_bytes(), Some(300 * 1024 * 1024));
}

#[test]
fn test_eh_max_archive_size_zero_disables_limit() {
    let cfg = EhentaiConfig {
        max_archive_size_mb: 0,
        ..Default::default()
    };
    assert_eq!(cfg.max_archive_size_bytes(), None);
}
```

Run: `cargo test -p pixivbot config::tests::test_eh_max_archive_size -- --nocapture`

Expected: FAIL because `max_archive_size_mb` and `max_archive_size_bytes()` do not exist.

- [ ] **Step 2: Implement config field/helper**

In `EhentaiConfig`, add after `download_rate_window_hours`:

```rust
/// Maximum EH gallery metadata size allowed for logged-in archive downloads, in MiB.
/// `0` disables this per-gallery archive gate.
#[serde(default = "default_eh_max_archive_size_mb")]
pub max_archive_size_mb: u64,
```

Set it in `Default for EhentaiConfig`:

```rust
max_archive_size_mb: default_eh_max_archive_size_mb(),
```

Add helper:

```rust
pub fn max_archive_size_bytes(&self) -> Option<u64> {
    if self.max_archive_size_mb == 0 {
        None
    } else {
        Some(self.max_archive_size_mb.saturating_mul(1024 * 1024))
    }
}
```

Add default function near other EH defaults:

```rust
fn default_eh_max_archive_size_mb() -> u64 {
    300
}
```

Run: `cargo test -p pixivbot config::tests::test_eh_max_archive_size -- --nocapture`

Expected: PASS.

- [ ] **Step 3: Document config**

In `config.toml.example`, under `[ehentai]` near download rate settings, add:

```toml
# Maximum EH gallery metadata size for logged-in archive downloads, in MiB.
# The check runs before the archive request that can spend EH archive points.
# Set to 0 to disable this per-gallery limit.
max_archive_size_mb = 300
```

- [ ] **Step 4: Add size guard helper**

In `src/scheduler/eh_engine.rs`, add a helper near the other top-level helper functions:

```rust
fn format_mib(bytes: u64) -> u64 {
    bytes.div_ceil(1024 * 1024)
}

async fn ensure_eh_archive_under_size_limit(
    client: &EhClient,
    config: &EhentaiConfig,
    gid: u64,
    token: &str,
) -> Result<()> {
    let Some(limit_bytes) = config.max_archive_size_bytes() else {
        return Ok(());
    };

    let metadata = client
        .get_metadata(&[(gid, token)])
        .await
        .context("Failed to fetch EH metadata for archive size check")?;
    let Some(gallery) = metadata.first() else {
        return Ok(());
    };
    if gallery.filesize == 0 || gallery.filesize <= limit_bytes {
        return Ok(());
    }

    anyhow::bail!(
        "EH gallery archive is too large: {} MiB exceeds configured {} MiB limit",
        format_mib(gallery.filesize),
        format_mib(limit_bytes)
    );
}
```

Call it in both logged-in archive paths before `prepare_archive_download()`:

```rust
ensure_eh_archive_under_size_limit(&self.client, &self.config, gid, token).await?;
let archive_request = self
    .client
    .prepare_archive_download(gid, token, resolution)
    .await
    .context("Failed to prepare archive download")?;
```

Run: `cargo test -p pixivbot config::tests::test_eh_max_archive_size -- --nocapture`

Expected: PASS and compile the new helper.

## Task 2: ipfS3 ZIP upload capability

**Files:**
- Modify: `eh_client/src/telegraph.rs`
- Modify: `eh_client/src/lib.rs`
- Modify: `config.toml.example`

**Interfaces:**
- Consumes: `IpfS3Uploader::url_pair_for_cid(&self, cid: &str) -> TelegraphImageUrlPair`, `public_url_for_key(public_base_url, key)` segment encoding style.
- Produces: `ZipArchiveUploadInput<'a>`, `ImageUploader::upload_zip_archive_with_url_pairs(...) -> Result<Option<Vec<TelegraphImageUrlPair>>>`, and ipfS3 ZIP implementation.

- [ ] **Step 1: Add failing tests**

Add tests in `eh_client/src/telegraph.rs` test module:

```rust
#[test]
fn ipfs3_zip_extract_disabled_by_default() {
    let config = IpfS3UploaderConfig::default();
    assert!(!config.zip_extract_enabled);
}

#[test]
fn ipfs3_zip_extract_entry_url_pairs_encode_each_path_segment() {
    let config = ResolvedIpfS3UploaderConfig {
        endpoint_url: "https://s3.example".into(),
        bucket: "bucket".into(),
        region: "auto".into(),
        access_key_id: "ak".into(),
        secret_access_key: "sk".into(),
        gateway_url: "https://public.example/ipfs".into(),
        preview_gateway_url: Some("https://preview.example/ipfs".into()),
        preview_rewrite_delay_sec: 600,
        key_prefix: "eh".into(),
        path_style: true,
        warm_public_gateway_after_upload: false,
        zip_extract_enabled: true,
    };

    let pairs = ipfs3_zip_entry_url_pairs(
        &config,
        "bafyRoot",
        &["001 cover.jpg".to_string(), "dir/page#2.png".to_string()],
    );

    assert_eq!(pairs[0].preview_url, "https://preview.example/ipfs/bafyRoot/001%20cover.jpg");
    assert_eq!(pairs[0].public_url, "https://public.example/ipfs/bafyRoot/001%20cover.jpg");
    assert_eq!(pairs[1].preview_url, "https://preview.example/ipfs/bafyRoot/dir/page%232.png");
    assert_eq!(pairs[1].public_url, "https://public.example/ipfs/bafyRoot/dir/page%232.png");
}
```

Run: `cargo test -p eh_client ipfs3_zip_extract -- --nocapture`

Expected: FAIL because the config field/helper do not exist.

- [ ] **Step 2: Add config and trait types**

Add to `IpfS3UploaderConfig`:

```rust
#[serde(default)]
pub zip_extract_enabled: bool,
```

Add to `ResolvedIpfS3UploaderConfig` and `required()`:

```rust
zip_extract_enabled: self.zip_extract_enabled,
```

Add near `ImageUploadInput`:

```rust
pub struct ZipArchiveUploadInput<'a> {
    pub filename: &'a str,
    pub bytes: &'a [u8],
    pub entry_names: &'a [String],
}
```

Add default trait method:

```rust
async fn upload_zip_archive_with_url_pairs(
    &self,
    _archive: ZipArchiveUploadInput<'_>,
) -> Result<Option<Vec<TelegraphImageUrlPair>>> {
    Ok(None)
}
```

In `eh_client/src/lib.rs`, add `ZipArchiveUploadInput` to the `pub use telegraph::{ ... }` list:

```rust
pub use telegraph::{
    rewrite_ipfs_gateway_nodes, CatboxUploader, CatboxUploaderConfig, ImageUploadConfig,
    ImageUploadInput, ImageUploadProvider, ImageUploader, IpfS3PreviewRewriteConfig, IpfS3Uploader,
    IpfS3UploaderConfig, PixiUploader, S3Uploader, S3UploaderConfig, TelegraphClient,
    TelegraphGalleryPageResult, TelegraphImageUrlPair, TelegraphRewriteData, TelegraphRewritePage,
    ZipArchiveUploadInput,
};
```

- [ ] **Step 3: Implement URL pair helper and ipfS3 ZIP upload**

Add helper near `public_url_for_key`:

```rust
fn gateway_url_for_zip_entry(gateway_url: &str, cid: &str, entry_name: &str) -> String {
    let encoded_entry = entry_name
        .split('/')
        .map(urlencoding::encode)
        .collect::<Vec<_>>()
        .join("/");
    format!("{}/{cid}/{encoded_entry}", gateway_url.trim_end_matches('/'))
}

fn ipfs3_zip_entry_url_pairs(
    config: &ResolvedIpfS3UploaderConfig,
    cid: &str,
    entry_names: &[String],
) -> Vec<TelegraphImageUrlPair> {
    let preview_gateway = config.preview_gateway_url.as_deref().unwrap_or(&config.gateway_url);
    entry_names
        .iter()
        .map(|entry| TelegraphImageUrlPair {
            preview_url: gateway_url_for_zip_entry(preview_gateway, cid, entry),
            public_url: gateway_url_for_zip_entry(&config.gateway_url, cid, entry),
        })
        .collect()
}
```

Add to `impl IpfS3Uploader`:

```rust
fn archive_object_key(&self, input: &ZipArchiveUploadInput<'_>) -> String {
    let timestamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
    let hash = short_hash_hex(input.bytes);
    let filename = format!("{timestamp}-archive-{hash}.zip");
    if self.config.key_prefix.is_empty() {
        filename
    } else {
        format!("{}/{}", self.config.key_prefix, filename)
    }
}

pub async fn upload_zip_archive_with_url_pairs(
    &self,
    archive: ZipArchiveUploadInput<'_>,
) -> Result<Option<Vec<TelegraphImageUrlPair>>> {
    if !self.config.zip_extract_enabled {
        return Ok(None);
    }
    if archive.entry_names.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let key = self.archive_object_key(&archive);
    let response = self
        .bucket
        .put_object_with_content_type(&key, archive.bytes, "application/zip")
        .await
        .map_err(|e| Error::Other(format!("ipfS3 ZIP put_object failed for key {key}: {e}")))?;
    let status = response.status_code();
    if !(200..300).contains(&status) {
        return Err(Error::Api {
            message: format!("ipfS3 ZIP put_object returned {status} for key {key}"),
            status,
        });
    }
    let cid = response
        .as_str()
        .map_err(|e| Error::Other(format!("ipfS3 ZIP put_object for key {key} returned non-UTF-8 ETag: {e}")))?
        .trim_matches('"')
        .trim();
    if cid.is_empty() {
        return Err(Error::Other(format!(
            "ipfS3 ZIP put_object for key {key} returned no ETag (CID); cannot build public URL"
        )));
    }
    self.warm_public_gateway(cid);
    Ok(Some(ipfs3_zip_entry_url_pairs(&self.config, cid, archive.entry_names)))
}
```

Override the trait method in `impl ImageUploader for IpfS3Uploader` by delegating to the inherent method.

Run: `cargo test -p eh_client ipfs3_zip_extract -- --nocapture`

Expected: PASS.

- [ ] **Step 4: Add disabled capability and missing-CID tests**

Add a small default trait behavior test:

```rust
struct DefaultZipCapabilityUploader;

#[async_trait::async_trait]
impl ImageUploader for DefaultZipCapabilityUploader {
    async fn upload_images(&self, _images: &[ImageUploadInput<'_>]) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn default_zip_archive_upload_capability_returns_none() {
    let uploader = DefaultZipCapabilityUploader;
    let entries = vec!["page001.jpg".to_string()];
    let result = uploader
        .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
            filename: "gallery.zip",
            bytes: b"zip bytes",
            entry_names: &entries,
        })
        .await
        .unwrap();

    assert!(result.is_none());
}
```

Add a concrete ipfS3 disabled-capability test so the real uploader implementation is covered:

```rust
#[tokio::test]
async fn ipfs3_zip_archive_upload_disabled_returns_none_without_put() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("PUT"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("unexpected-cid"))
        .expect(0)
        .mount(&server)
        .await;

    let uploader = IpfS3Uploader::from_config(&IpfS3UploaderConfig {
        endpoint_url: Some(server.uri()),
        bucket: Some("bucket".into()),
        region: Some("auto".into()),
        access_key_id: Some("ak".into()),
        secret_access_key: Some("sk".into()),
        gateway_url: Some("https://public.example/ipfs".into()),
        zip_extract_enabled: false,
        path_style: true,
        ..Default::default()
    })
    .unwrap();
    let entries = vec!["page001.jpg".to_string()];

    let result = uploader
        .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
            filename: "gallery.zip",
            bytes: b"zip bytes",
            entry_names: &entries,
        })
        .await
        .unwrap();

    assert!(result.is_none());
}
```

Add a wiremock-backed ipfS3 missing-CID test using the same S3-compatible request style already used by existing S3/ipfS3 tests in this file. The assertion must prove a 2xx response with an empty ETag/body returns an error containing `returned no ETag (CID)`:

```rust
#[tokio::test]
async fn ipfs3_zip_archive_upload_rejects_empty_cid() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("PUT"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(""))
        .expect(1)
        .mount(&server)
        .await;

    let uploader = IpfS3Uploader::from_config(&IpfS3UploaderConfig {
        endpoint_url: Some(server.uri()),
        bucket: Some("bucket".into()),
        region: Some("auto".into()),
        access_key_id: Some("ak".into()),
        secret_access_key: Some("sk".into()),
        gateway_url: Some("https://public.example/ipfs".into()),
        zip_extract_enabled: true,
        path_style: true,
        ..Default::default()
    })
    .unwrap();
    let entries = vec!["page001.jpg".to_string()];

    let err = uploader
        .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
            filename: "gallery.zip",
            bytes: b"zip bytes",
            entry_names: &entries,
        })
        .await
        .unwrap_err();

    assert!(err.to_string().contains("returned no ETag (CID)"));
}
```

Run: `cargo test -p eh_client zip_archive_upload -- --nocapture`

Expected: PASS. If the rust-s3 test request path needs a more specific `path("/bucket/<key>")` matcher, keep the method matcher broad and assert `.expect(1)` so the test remains focused on ETag/CID validation rather than key timestamp details.

- [ ] **Step 5: Document ipfS3 flag**

In `config.toml.example`, under `[image_upload.ipfs3]`, add:

```toml
# If your ipfS3 deployment expands uploaded ZIP archives into addressable files
# under the returned root CID, EH Telegraph uploads can upload the ZIP once and
# use {gateway}/{CID}/{zip-entry-path} URLs instead of uploading every image.
# Leave disabled unless your provider supports this behavior.
zip_extract_enabled = false
```

## Task 3: EH upload worker ZIP-first path

**Files:**
- Modify: `src/scheduler/eh_engine.rs`

**Interfaces:**
- Consumes: `ZipArchiveUploadInput<'_>` and `ImageUploader::upload_zip_archive_with_url_pairs(...)` from Task 2.
- Produces: `collect_uploadable_zip_entry_names(path: &Path) -> Result<Vec<String>>` and ZIP-first upload branch in `EhUploadWorker::process`.

- [ ] **Step 1: Add failing mock uploader test**

Add a mock in `src/scheduler/eh_engine.rs` integration tests:

```rust
#[derive(Default)]
struct ZipFirstMockUploader {
    zip_calls: std::sync::atomic::AtomicUsize,
    image_calls: std::sync::atomic::AtomicUsize,
    seen_entries: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ImageUploader for ZipFirstMockUploader {
    async fn upload_images(&self, _images: &[ImageUploadInput<'_>]) -> eh_client::Result<Vec<String>> {
        self.image_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Vec::new())
    }

    async fn upload_zip_archive_with_url_pairs(
        &self,
        archive: eh_client::ZipArchiveUploadInput<'_>,
    ) -> eh_client::Result<Option<Vec<TelegraphImageUrlPair>>> {
        self.zip_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *self.seen_entries.lock().unwrap() = archive.entry_names.to_vec();
        Ok(Some(
            archive
                .entry_names
                .iter()
                .map(|name| TelegraphImageUrlPair {
                    preview_url: format!("https://preview.example/ipfs/root/{name}"),
                    public_url: format!("https://public.example/ipfs/root/{name}"),
                })
                .collect(),
        ))
    }
}
```

Add test:

```rust
#[tokio::test]
async fn test_upload_worker_prefers_zip_archive_uploader() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let tg_server = MockServer::start().await;
    mock_telegraph_create_page(&tg_server).await;
    let notifier = make_notifier(&tg_server);
    let temp_dir = tempfile::tempdir().unwrap();
    let zip_path = temp_dir.path().join("zip_first.zip");
    create_test_zip(&zip_path, 2);
    let entry = insert_queue_entry(
        &repo,
        -100,
        700,
        "tok",
        "Title",
        true,
        STATUS_DOWNLOADED,
        Some(zip_path.to_str().unwrap()),
        None,
    )
    .await;
    let uploader = Arc::new(ZipFirstMockUploader::default());
    let worker = EhUploadWorker::new(
        Arc::clone(&repo),
        notifier,
        make_telegraph_client(&tg_server),
        uploader.clone(),
        None,
        Arc::new(make_config()),
    );

    worker.tick().await.unwrap();

    assert_eq!(uploader.zip_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(uploader.image_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(
        *uploader.seen_entries.lock().unwrap(),
        vec!["page000.jpg".to_string(), "page001.jpg".to_string()]
    );
    let model = eh_download_queue::Entity::find_by_id(entry.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(model.status, STATUS_UPLOADED);
}
```

Run: `cargo test -p pixivbot test_upload_worker_prefers_zip_archive_uploader -- --nocapture`

Expected: FAIL because the trait method and worker branch are not used yet.

- [ ] **Step 2: Add ZIP entry-name collection helper**

In `src/scheduler/eh_engine.rs`, add:

```rust
fn collect_uploadable_zip_entry_names(zip_path: &std::path::Path) -> Result<Vec<String>> {
    let zip_file = std::fs::File::open(zip_path).context("Failed to open zip")?;
    let mut archive = zip::ZipArchive::new(zip_file).context("Failed to read zip archive")?;
    let mut names = Vec::new();
    for i in 0..archive.len() {
        let file = archive.by_index(i).context("Failed to read zip entry")?;
        let name = file.name().replace('\\', "/");
        if is_uploadable_zip_image_name(&name.to_lowercase()) {
            names.push(name);
        }
    }
    Ok(names)
}
```

- [ ] **Step 3: Use ZIP uploader before per-image path**

At the start of `EhUploadWorker::process`, after `zip_path` is resolved and before creating the image channel, add:

```rust
let entry_names = collect_uploadable_zip_entry_names(zip_path)?;
if entry_names.is_empty() {
    anyhow::bail!("No images found in downloaded EH ZIP");
}
if self.image_uploader.supports_zip_archive_upload() {
    let zip_bytes = tokio::fs::read(zip_path)
        .await
        .context("Failed to read zip for archive upload")?;
    let archive_input = eh_client::ZipArchiveUploadInput {
        filename: zip_path.file_name().and_then(|n| n.to_str()).unwrap_or("gallery.zip"),
        bytes: zip_bytes.as_slice(),
        entry_names: &entry_names,
    };
    if let Some(url_pairs) = self
        .image_uploader
        .upload_zip_archive_with_url_pairs(archive_input)
        .await
        .context("Failed to upload EH ZIP archive for Telegraph page")?
    {
        if url_pairs.len() != entry_names.len() {
            anyhow::bail!(
                "ZIP archive uploader returned {} URLs for {} image entries",
                url_pairs.len(),
                entry_names.len()
            );
        }
        self.create_telegraph_page_for_entry(entry, &url_pairs).await?;
        return Ok(());
    }
}
```

Extract the existing Telegraph page creation and repo mark block into:

```rust
async fn create_telegraph_page_for_entry(
    &self,
    entry: &eh_download_queue::Model,
    all_url_pairs: &[TelegraphImageUrlPair],
) -> Result<()> { /* move existing title/create/serialize/mark block here */ }
```

Then replace the old inline block with `self.create_telegraph_page_for_entry(entry, &all_url_pairs).await?;`.

Run: `cargo test -p pixivbot test_upload_worker_prefers_zip_archive_uploader -- --nocapture`

Expected: PASS.

## Task 4: Download guard integration tests and final verification

**Files:**
- Modify: `src/scheduler/eh_engine.rs`
- Modify if needed: `eh_client/src/telegraph.rs`
- Modify if needed: `src/config.rs`

**Interfaces:**
- Consumes: helpers from Tasks 1-3.
- Produces: test evidence that oversize galleries do not hit archive POST and limit equality still allows download.

- [ ] **Step 1: Add EH metadata mock helper**

In `src/scheduler/eh_engine.rs` integration tests, add:

```rust
async fn mock_eh_metadata(server: &MockServer, gid: u64, token: &str, filesize: u64) {
    let body = serde_json::json!({
        "gmetadata": [{
            "gid": gid,
            "token": token,
            "archiver_key": "",
            "title": "Size Test Gallery",
            "title_jpn": "",
            "category": "Doujinshi",
            "thumb": "",
            "uploader": "tester",
            "posted": "1700000000",
            "filecount": "2",
            "filesize": filesize,
            "rating": "4.5",
            "torrentcount": "0",
            "tags": []
        }]
    });
    Mock::given(method("POST"))
        .and(path("/api.php"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(server)
        .await;
}
```

- [ ] **Step 2: Add oversized download test**

Add:

```rust
#[tokio::test]
async fn test_download_size_limit_blocks_before_archive_post() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let eh_server = MockServer::start().await;
    let temp_dir = tempfile::tempdir().unwrap();
    mock_eh_metadata(&eh_server, 900, "tok", 301 * 1024 * 1024).await;
    Mock::given(method("POST"))
        .and(path("/archiver.php"))
        .respond_with(ResponseTemplate::new(200).set_body_string("unexpected"))
        .expect(0)
        .mount(&eh_server)
        .await;
    let mut cfg = make_config();
    cfg.max_archive_size_mb = 300;
    cfg.max_retry_count = 0;
    let entry = insert_queue_entry(&repo, -100, 900, "tok", "Title", false, STATUS_PENDING, None, None).await;
    let worker = EhDownloadWorker::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(cfg),
        temp_dir.path().to_path_buf(),
    );

    worker.tick().await.unwrap();

    let model = eh_download_queue::Entity::find_by_id(entry.id)
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(model.status, STATUS_FAILED);
    assert!(model.error.unwrap().contains("exceeds configured 300 MiB limit"));
}
```

Run: `cargo test -p pixivbot test_download_size_limit_blocks_before_archive_post -- --nocapture`

Expected: PASS after Task 1 implementation.

- [ ] **Step 3: Add equality allowed test**

Add a similar test with `filesize = 300 * 1024 * 1024`, plus `mock_eh_gallery_page`, `mock_eh_archiver_post`, and `mock_eh_archive_download`, asserting final status is `STATUS_DOWNLOADED`. Use `create_test_zip` to generate download bytes.

Run: `cargo test -p pixivbot test_download_size_limit_allows_equal_size -- --nocapture`

Expected: PASS.

- [ ] **Step 4: Add shared size guard coverage for background path**

Because both `EhDownloadWorker` and `EhBackgroundDownloadWorker` call the same `ensure_eh_archive_under_size_limit()` helper before their archive request, add a direct helper test that proves the shared guard blocks oversize metadata before any archiver page or POST mock is needed:

```rust
#[tokio::test]
async fn test_shared_size_limit_guard_rejects_oversized_metadata() {
    let eh_server = MockServer::start().await;
    mock_eh_metadata(&eh_server, 902, "tok", 301 * 1024 * 1024).await;
    let mut cfg = make_config();
    cfg.max_archive_size_mb = 300;

    let err = ensure_eh_archive_under_size_limit(
        make_eh_client(&eh_server).as_ref(),
        &cfg,
        902,
        "tok",
    )
    .await
    .unwrap_err();

    assert!(err.to_string().contains("exceeds configured 300 MiB limit"));
}
```

Run: `cargo test -p pixivbot test_shared_size_limit_guard_rejects_oversized_metadata -- --nocapture`

Expected: PASS. This covers the shared helper used by both foreground and background archive download paths.

- [ ] **Step 5: Run focused and broad verification**

Run:

```powershell
cargo test -p pixivbot config::tests::test_eh_max_archive_size -- --nocapture
cargo test -p eh_client ipfs3_zip_extract -- --nocapture
cargo test -p eh_client zip_archive_upload -- --nocapture
cargo test -p pixivbot test_upload_worker_prefers_zip_archive_uploader -- --nocapture
cargo test -p pixivbot test_download_size_limit -- --nocapture
cargo test -p pixivbot test_shared_size_limit_guard_rejects_oversized_metadata -- --nocapture
make quick
git diff --check -- src/config.rs config.toml.example src/scheduler/eh_engine.rs eh_client/src/telegraph.rs eh_client/src/lib.rs docs/superpowers/specs/2026-07-18-eh-zip-size-and-ipfs3-design.md docs/superpowers/plans/2026-07-18-eh-zip-size-and-ipfs3.md
```

Expected: all commands exit 0. If `make quick` fails due missing local FFmpeg/pkg-config dependencies, record the dependency error and still run the focused cargo tests that do not require unavailable native libs.

## Self-review

- Spec coverage: Task 1 covers config/default/disable and pre-POST size gate; Task 2 covers optional configured ipfS3 ZIP upload; Task 3 wires EH upload ZIP-first fallback behavior; Task 4 covers no POST on oversize, equality allowed, and verification.
- Placeholder scan: no TBD/TODO/fill-in-later instructions remain; code snippets provide exact interfaces and commands.
- Type consistency: `max_archive_size_bytes`, `ZipArchiveUploadInput`, `upload_zip_archive_with_url_pairs`, and `TelegraphImageUrlPair` names are consistent across tasks.
