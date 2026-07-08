# IPFS Dual-Gateway Telegraph Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Create Telegraph pages with preview-friendly IPFS gateway URLs, then rewrite them to public gateway URLs after the Telegraph link has been sent for 10 minutes.

**Architecture:** Keep upload and publish decoupled. The ipfS3 uploader emits preview/public URL pairs for each CID, Telegraph page creation stores preview URLs and returns rewrite metadata, and a DB-backed rewrite worker edits due Telegraph pages after publish. The feature is disabled unless `preview_gateway_url` differs from the public `gateway_url`.

**Tech Stack:** Rust 1.94, SeaORM migrations/entities, teloxide scheduler workers, `eh_client` Telegraph API client, serde JSON, reqwest/wiremock tests.

---

## File map

- Modify `eh_client/src/telegraph.rs`: gateway URL pair types, `editPage`, gallery creation result, node rewrite helpers, tests.
- Modify `eh_client/src/telegraph.rs`: new ipfS3 preview gateway fields, `IpfS3PreviewRewriteConfig`, URL pair helpers, `editPage`, gallery creation result, node rewrite helpers, tests.
- Modify `src/config.rs` and `config.toml.example`: thread the `eh_client::ImageUploadConfig::ipfs3_preview_rewrite_config()` result into scheduler workers and document settings.
- Create migration in `migration/src/m20260707_000001_add_eh_telegraph_rewrite.rs`; modify `migration/src/lib.rs`.
- Modify generated DB entity `src/db/entities/eh_download_queue.rs` if entities are maintained manually in this repo.
- Modify `src/db/repo.rs`: add rewrite columns to the manually created test schema.
- Modify `src/db/repo/eh_download_queue.rs`: persist rewrite metadata, atomically claim due rewrite jobs, recover stale rewrite jobs, clear stale rewrite state, mark success/failure.
- Modify `src/scheduler/eh_engine.rs`: receive resolved ipfS3 rewrite settings, store rewrite metadata after upload, schedule rewrite after publish, add `EhTelegraphRewriteWorker`.
- Modify `src/main.rs`: spawn rewrite worker when Telegraph client exists.

## Task 1: Telegraph URL pairs and editPage

**Files:**
- Modify: `eh_client/src/telegraph.rs`

- [ ] **Step 1: Add URL pair and rewrite metadata types**

Add near `ImageUploadInput`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TelegraphImageUrlPair {
    pub preview_url: String,
    pub public_url: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TelegraphRewritePage {
    pub path: String,
    pub title: String,
    pub content: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TelegraphRewriteData {
    pub pages: Vec<TelegraphRewritePage>,
    pub preview_gateway_url: String,
    pub public_gateway_url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TelegraphGalleryPageResult {
    pub first_page_url: String,
    pub rewrite_data: Option<TelegraphRewriteData>,
}
```

If `Node` is not serializable, derive `Serialize`/`Deserialize` on it and its attribute structs. Keep existing JSON wire format unchanged.

- [ ] **Step 2: Add failing node rewrite tests**

Add tests in `eh_client/src/telegraph.rs`:

```rust
#[test]
fn rewrite_nodes_replaces_only_preview_image_srcs() {
    let nodes = vec![
        Node::img("https://preview.example/ipfs/cid1"),
        Node::link("https://telegra.ph/next", "Next Page →"),
        Node::img("https://public.example/ipfs/cid2"),
    ];
    let rewritten = rewrite_ipfs_gateway_nodes(
        &nodes,
        "https://preview.example/ipfs",
        "https://public.example/ipfs",
    );

    assert_eq!(node_attr_str(&rewritten[0], "src"), Some("https://public.example/ipfs/cid1"));
    assert_eq!(node_attr_str(&rewritten[1], "href"), Some("https://telegra.ph/next"));
    assert_eq!(node_attr_str(&rewritten[2], "src"), Some("https://public.example/ipfs/cid2"));
}
```

Run:

```powershell
cargo test -p eh_client rewrite_nodes_replaces_only_preview_image_srcs
```

Expected: FAIL because `rewrite_ipfs_gateway_nodes()` does not exist.

- [ ] **Step 3: Implement node rewrite helper**

Add this public helper plus a small attribute accessor used by tests and the worker:

```rust
pub fn node_attr_str<'a>(node: &'a Node, key: &str) -> Option<&'a str> {
    node.attrs.as_ref()?.get(key)?.as_str()
}

fn set_node_attr_string(node: &mut Node, key: &str, value: String) {
    if let Some(attrs) = node.attrs.as_mut() {
        attrs.insert(key.to_string(), serde_json::Value::String(value));
    }
}

pub fn rewrite_ipfs_gateway_nodes(nodes: &[Node], preview_gateway_url: &str, public_gateway_url: &str) -> Vec<Node> {
    nodes
        .iter()
        .cloned()
        .map(|mut node| {
            if node.tag == "img" {
                let prefix = format!("{}/", preview_gateway_url.trim_end_matches('/'));
                if let Some(cid) = node_attr_str(&node, "src").and_then(|src| src.strip_prefix(&prefix)) {
                    set_node_attr_string(
                        &mut node,
                        "src",
                        format!("{}/{}", public_gateway_url.trim_end_matches('/'), cid),
                    );
                }
            }
            node
        })
        .collect()
}
```

Run the test again; expected PASS.

- [ ] **Step 4: Add editPage client test**

Add a wiremock test:

```rust
#[tokio::test]
async fn edit_page_posts_path_title_and_content() {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/editPage"))
        .and(body_string_contains("access_token=test-token"))
        .and(body_string_contains("path=Page-01"))
        .and(body_string_contains("title=Title"))
        .and(body_string_contains("content="))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": { "url": "https://telegra.ph/Page-01" }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = TelegraphClient::new_with_urls(
        "test-token".to_string(),
        "https://pixi.example/api".to_string(),
        server.uri(),
    );
    client.edit_page("Page-01", "Title", &[Node::img("https://public.example/ipfs/cid")]).await.unwrap();
}
```

Run:

```powershell
cargo test -p eh_client edit_page_posts_path_title_and_content
```

Expected: FAIL because `edit_page` is missing.

- [ ] **Step 5: Implement `TelegraphClient::edit_page()`**

Add method mirroring `create_page()`:

```rust
pub async fn edit_page(&self, path: &str, title: &str, content: &[Node]) -> Result<String> {
    let content_json = serde_json::to_string(content)?;
    let resp = self.http
        .post(format!("{}/editPage", self.api_url))
        .form(&[
            ("access_token", self.telegraph_token.as_str()),
            ("path", path),
            ("title", title),
            ("content", content_json.as_str()),
            ("return_content", "false"),
        ])
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::Api { message: format!("editPage returned {}", status), status: status.as_u16() });
    }
    let telegraph_resp: TelegraphResponse<PageResult> = resp.json().await?;
    if telegraph_resp.ok {
        if let Some(result) = telegraph_resp.result {
            return Ok(result.url);
        }
    }
    Err(Error::Api { message: telegraph_resp.error.unwrap_or_else(|| "unknown error".into()), status: 0 })
}
```

Run the edit test again; expected PASS.

## Task 2: Dual gateway URL generation

**Files:**
- Modify: `eh_client/src/telegraph.rs`
- Modify: `src/config.rs`
- Modify: `config.toml.example`

- [ ] **Step 1: Add config fields and normalization tests**

Add fields to `IpfS3UploaderConfig` in `eh_client/src/telegraph.rs`:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub preview_gateway_url: Option<String>,
#[serde(default = "default_ipfs_preview_rewrite_delay_sec")]
pub preview_rewrite_delay_sec: u64,
```

Add helper:

```rust
fn default_ipfs_preview_rewrite_delay_sec() -> u64 { 600 }
```

Add tests that `gateway_url = "https://ipfs.io/ipfs/"` and `preview_gateway_url = "https://ipfs-gw.moyuteam.me/ipfs/"` normalize without trailing slash.

Add a resolved settings type in `eh_client/src/telegraph.rs` and re-export it from `eh_client/src/lib.rs` because `src/main.rs` and `src/scheduler/eh_engine.rs` will use it:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpfS3PreviewRewriteConfig {
    pub preview_gateway_url: String,
    pub public_gateway_url: String,
    pub delay_sec: u64,
}

impl ImageUploadConfig {
    pub fn ipfs3_preview_rewrite_config(&self) -> Option<IpfS3PreviewRewriteConfig> {
        let ipfs3 = self.ipfs3.as_ref()?;
        let public = ipfs3.gateway_url.trim_end_matches('/').to_string();
        let preview = ipfs3.preview_gateway_url.as_ref()?.trim_end_matches('/').to_string();
        if preview == public {
            return None;
        }
        Some(IpfS3PreviewRewriteConfig {
            preview_gateway_url: preview,
            public_gateway_url: public,
            delay_sec: ipfs3.preview_rewrite_delay_sec,
        })
    }
}
```

`src/main.rs` calls `config.image_upload.ipfs3_preview_rewrite_config()` on the real `eh_client::ImageUploadConfig` value and passes the resulting `Option<IpfS3PreviewRewriteConfig>` into `EhUploadWorker` and `EhPublishWorker`; workers do not read `ImageUploadConfig` directly.

- [ ] **Step 2: Emit preview/public URL pairs from ipfS3**

Add method to `IpfS3Uploader`:

```rust
fn url_pair_for_cid(&self, cid: &str) -> TelegraphImageUrlPair {
    let public_url = format!("{}/{}", self.config.gateway_url, cid);
    let preview_url = self
        .config
        .preview_gateway_url
        .as_ref()
        .map(|gateway| format!("{}/{}", gateway, cid))
        .unwrap_or_else(|| public_url.clone());
    TelegraphImageUrlPair { preview_url, public_url }
}
```

Keep `ImageUploader::upload_images()` returning preview URLs so existing callers work. Add an optional downcast-free method on `IpfS3Uploader`:

```rust
pub async fn upload_images_with_url_pairs(&self, images: &[ImageUploadInput<'_>]) -> Result<Vec<TelegraphImageUrlPair>>
```

This method shares the same PUT logic as `upload_images()`, but returns pairs. `upload_images()` maps pairs to `preview_url`.

`EhUploadWorker` keeps using `Arc<dyn ImageUploader>`. When `IpfS3PreviewRewriteConfig` is present, it converts each returned preview URL into a pair by replacing the preview gateway prefix with the public gateway prefix:

```rust
fn preview_url_to_pair(url: String, rewrite: &IpfS3PreviewRewriteConfig) -> Option<TelegraphImageUrlPair> {
    let prefix = format!("{}/", rewrite.preview_gateway_url.trim_end_matches('/'));
    let cid = url.strip_prefix(&prefix)?;
    Some(TelegraphImageUrlPair {
        preview_url: url,
        public_url: format!("{}/{}", rewrite.public_gateway_url.trim_end_matches('/'), cid),
    })
}
```

If any uploaded URL does not match the preview gateway prefix, log a warning and disable rewrite metadata for that gallery rather than failing the upload.

- [ ] **Step 3: Document config**

Update `config.toml.example` under `[image_upload.ipfs3]`:

```toml
# Public/final IPFS gateway used after Telegraph preview warm-up.
# gateway_url = "https://ipfs.io/ipfs"
# Optional preview gateway used only while Telegram fetches Telegraph preview.
# preview_gateway_url = "https://ipfs-gw.moyuteam.me/ipfs"
# Delay after sending the Telegraph link before rewriting preview URLs to gateway_url.
# preview_rewrite_delay_sec = 600
```

## Task 3: Gallery creation result and rewrite metadata

**Files:**
- Modify: `eh_client/src/telegraph.rs`

- [ ] **Step 1: Add failing multi-page metadata test**

Add test:

```rust
#[tokio::test]
async fn create_gallery_page_with_rewrite_data_tracks_all_pages() {
    let pairs = vec![
        TelegraphImageUrlPair { preview_url: "https://preview/ipfs/a".into(), public_url: "https://public/ipfs/a".into() },
        TelegraphImageUrlPair { preview_url: "https://preview/ipfs/b".into(), public_url: "https://public/ipfs/b".into() },
    ];
    let result = TelegraphClient::create_gallery_page_nodes_for_test(
        "Title",
        &pairs,
        "https://preview/ipfs",
        "https://public/ipfs",
    );
    assert!(result.rewrite_data.is_some());
    let rewrite = result.rewrite_data.unwrap();
    assert!(rewrite.pages.iter().all(|page| page.content.iter().any(|node| node.tag == "img")));
}
```

Expected: FAIL because helper/result is missing. If direct helper visibility is awkward, implement the same assertion around a private function inside the test module.

- [ ] **Step 2: Add `create_gallery_page_with_url_pairs()`**

Implement:

```rust
pub async fn create_gallery_page_with_url_pairs(
    &self,
    title: &str,
    image_urls: &[TelegraphImageUrlPair],
    preview_gateway_url: Option<&str>,
    public_gateway_url: Option<&str>,
) -> Result<TelegraphGalleryPageResult>
```

It creates pages using `preview_url`. If both gateways are present and different, it stores rewrite pages with preview nodes and page paths. Extract page path from returned URL with a helper:

```rust
fn telegraph_path_from_url(url: &str) -> Option<String> {
    reqwest::Url::parse(url).ok()?.path_segments()?.next_back().map(str::to_string)
}
```

Keep the existing `create_gallery_page(title, &[String]) -> Result<String>` as a wrapper that returns `first_page_url` and no rewrite metadata.

## Task 4: Persist rewrite metadata and schedule after publish

**Files:**
- Create: `migration/src/m20260707_000001_add_eh_telegraph_rewrite.rs`
- Modify: `migration/src/lib.rs`
- Modify: `src/db/entities/eh_download_queue.rs`
- Modify: `src/db/repo.rs`
- Modify: `src/db/repo/eh_download_queue.rs`
- Modify: `src/scheduler/eh_engine.rs`

- [ ] **Step 1: Add migration and entity fields**

Migration adds nullable columns:

```rust
telegraph_rewrite_after: timestamp null
telegraph_rewrite_data: text null
telegraph_rewritten_at: timestamp null
telegraph_rewrite_status: text null
telegraph_rewrite_started_at: timestamp null
telegraph_rewrite_next_retry_at: timestamp null
telegraph_rewrite_retry_count: integer not null default 0
telegraph_rewrite_error: text null
```

Register migration in `migration/src/lib.rs`.

Update entity model with matching `Option<DateTime>`, `Option<String>`, and retry-count fields. Update the manual in-memory schema in `src/db/repo.rs` with the same columns so repo/scheduler tests can insert and claim rewrite rows.

- [ ] **Step 2: Add repo methods**

Add methods:

```rust
pub async fn mark_eh_download_uploaded_with_rewrite(
    &self,
    id: i32,
    telegraph_url: &str,
    rewrite_data: Option<&TelegraphRewriteData>,
) -> Result<eh_download_queue::Model>

pub async fn schedule_eh_telegraph_rewrite_after_send(&self, id: i32, delay_secs: i64) -> Result<()>

pub async fn get_next_for_telegraph_rewrite(&self) -> Result<Option<eh_download_queue::Model>>

pub async fn mark_eh_telegraph_rewritten(&self, id: i32) -> Result<()>

pub async fn schedule_eh_telegraph_rewrite_retry_from(
    &self,
    id: i32,
    error: &str,
    max_retry_count: u8,
) -> Result<(eh_download_queue::Model, bool)>

pub async fn reset_stale_eh_telegraph_rewrites(&self, stale_after_secs: i64) -> Result<u64>
```

Rewrite lifecycle:

- `mark_eh_download_uploaded_with_rewrite()` stores `telegraph_rewrite_data` and leaves rewrite status null. No rewrite can run before the Telegraph link is sent.
- `schedule_eh_telegraph_rewrite_after_send()` is called after `mark_eh_telegraph_sent()`. If `telegraph_rewrite_data` is present, it sets `telegraph_rewrite_after = now + delay`, `telegraph_rewrite_status = "pending"`, clears `telegraph_rewrite_error`, and resets `telegraph_rewrite_retry_count = 0`.
- `get_next_for_telegraph_rewrite()` atomically claims one due row: select rows with `telegraph_rewrite_status = "pending"`, `telegraph_rewrite_after <= now`, `telegraph_rewrite_next_retry_at is null or <= now`, and `telegraph_rewritten_at is null`; update the selected row to `telegraph_rewrite_status = "rewriting"` and `telegraph_rewrite_started_at = now` with a status guard.
- `schedule_eh_telegraph_rewrite_retry_from()` only updates rows currently in `telegraph_rewrite_status = "rewriting"`. Transient failure sets status back to `pending`, increments retry count, stores error, clears started-at, and sets `telegraph_rewrite_next_retry_at` using `Repo::backoff_delay_secs()`. Permanent failure sets status `failed`, stores error, clears started-at, and does not alter the publish status or sent markers.
- `mark_eh_telegraph_rewritten()` only updates rows currently in `telegraph_rewrite_status = "rewriting"`; it sets `telegraph_rewritten_at = now`, clears `telegraph_rewrite_data`, `telegraph_rewrite_after`, `telegraph_rewrite_next_retry_at`, `telegraph_rewrite_started_at`, and `telegraph_rewrite_error`, and sets `telegraph_rewrite_status = null`.
- `reset_stale_eh_telegraph_rewrites()` runs at startup and changes `rewriting` rows older than the stale threshold back to `pending`, clears started-at, and preserves retry count/data.

Clear all rewrite fields whenever Telegraph state is scrubbed or made irrelevant: `fallback_eh_upload_to_archive`, `disable_eh_telegraph_for_unuploaded_entries`, `cancel_legacy_eh_subscription_queue_entries`, `cancel_eh_subscription_queue_entries_inner`, terminal row Telegraph-owner cleanup, and any re-enqueue/reset path that clears `telegraph_url` or sets `telegraph=false`. Do not clear rewrite fields in `mark_eh_download_done()` because delayed rewrite runs after publish completion.

- [ ] **Step 3: Store metadata after upload**

In `src/main.rs`, compute `let ipfs_preview_rewrite = config.image_upload.ipfs3_preview_rewrite_config();` and pass that option into `EhUploadWorker::new()` and `EhPublishWorker::new()`.

In `EhUploadWorker::process()`, keep uploading through the generic `ImageUploader`. When rewrite config is present, convert each returned preview URL into `TelegraphImageUrlPair` with `preview_url_to_pair()`. Call `create_gallery_page_with_url_pairs()` only when every URL can be paired. Otherwise call the existing `create_gallery_page()` wrapper. Store rewrite metadata via `mark_eh_download_uploaded_with_rewrite()`.

- [ ] **Step 4: Schedule rewrite after sending Telegraph link**

In `EhPublishWorker::process()`, immediately after `mark_eh_telegraph_sent(entry.id)`, call:

```rust
self.repo
    .schedule_eh_telegraph_rewrite_after_send(
        entry.id,
        self.ipfs_preview_rewrite.as_ref().map(|c| c.delay_sec).unwrap_or(600) as i64,
    )
    .await?;
```

Only schedule when rewrite metadata exists. The helper may no-op if `telegraph_rewrite_data` is null.

## Task 5: Rewrite worker

**Files:**
- Modify: `src/scheduler/eh_engine.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Add worker tests**

Add test that inserts a row with due `telegraph_rewrite_after`, `telegraph_rewrite_data` containing one page, mocks Telegraph `/editPage`, runs one worker tick, and asserts `telegraph_rewritten_at` is set and `telegraph_rewrite_data` is cleared.

- [ ] **Step 2: Implement worker**

Add `EhTelegraphRewriteWorker` with `run()` and `tick()` matching existing worker style. In `process(entry)`:

1. Deserialize `TelegraphRewriteData`.
2. For each page, call `rewrite_ipfs_gateway_nodes()` and `telegraph.edit_page(path, title, &rewritten_nodes)`.
3. Mark rewritten on success.

On `editPage` failure, call `schedule_eh_telegraph_rewrite_retry_from(entry.id, &e.to_string(), self.config.max_retry_count)`. Do not modify archive/publish status; this is post-publish cleanup.

- [ ] **Step 3: Spawn worker**

In `src/main.rs`, call `repo.reset_stale_eh_telegraph_rewrites(30 * 60).await` during EH startup after `reset_stale_eh_downloads()`. Spawn `EhTelegraphRewriteWorker` when `telegraph_client.is_some()` and EH is enabled.

## Task 6: Verification

Run:

```powershell
cargo test -p eh_client telegraph
cargo test -p pixivbot telegraph_rewrite
cargo test -p pixivbot scheduler::eh_engine::integration_tests::test_upload_worker
cargo fmt --all -- --check
git diff --check
$env:RUSTFLAGS = "-Dwarnings"; cargo clippy -p eh_client -p pixivbot --all-targets -- -D warnings
cargo check -p eh_client -p pixivbot --all-targets
```

Expected: all pass.

## Plan self-review

- Spec coverage: Covers preview/public gateway generation, Telegraph page metadata, delayed persistent rewrite, multi-page handling, config, DB, worker, and tests.
- Placeholder scan: No TBD/TODO placeholders remain.
- Type consistency: Uses `TelegraphImageUrlPair`, `TelegraphRewriteData`, `TelegraphGalleryPageResult`, and repo rewrite methods consistently.

## Execution handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-07-ipfs-dual-gateway-telegraph.md`.

Implementation should use subagent-driven development or inline execution with review checkpoints.
