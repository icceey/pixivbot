# EH Resolution Routing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Route EH downloads by requested resolution, and optionally downsample/convert Telegraph uploads to bounded JPEGs so Telegram link previews are more reliable.

**Architecture:** Add a typed EH resolution model in `eh_client`, treat `resample` as an alias for `1280x`, and make the scheduler ask the client to download the best available artifact for the requested use case. The first implementation keeps the existing one-ZIP-per-queue-row model: when `telegraph_resample` is disabled, every Telegraph queue entry, including `/edl ... telegraph=on`, uses `telegraph_resolution`; when `telegraph_resample` is enabled, Telegraph queue entries download using the normal direct/subscription resolution and the upload path converts non-JPEG images to JPEG and downscales images wider than the target width without upscaling smaller images.

**Tech Stack:** Rust 1.94, `eh_client`, PixivBot scheduler, SeaORM queue state, `image` crate for JPEG/PNG/GIF/WebP decoding and JPEG encoding, colocated Rust unit/integration tests.

---

## File Structure

- Modify `eh_client/src/client.rs`: add typed `EhResolution`, route `download_gallery_images` and archiver requests by resolution, expose a single `download_gallery_artifact()` entry point.
- Modify `eh_client/src/parser.rs`: parse archiver form availability and direct image page resolution choices.
- Modify `eh_client/tests/integration.rs`: add route-selection and fallback integration tests with wiremock.
- Modify `Cargo.toml`: ensure the `image` crate has decode features needed by uploadable EH image types.
- Modify `src/config.rs`: parse/validate configured EH resolutions, add `telegraph_resolution`, and add `telegraph_resample`.
- Modify `config.toml.example`: document the new resolution model accurately.
- Modify `src/scheduler/eh_engine.rs`: choose the requested resolution for a queue entry, call the new client routing API, and optionally resample Telegraph upload images before sending them to the configured image host.
- Modify `src/bot/handlers/subscription/ehentai.rs` only if direct commands need user-facing validation text updates.

## Task 1: Define the EH resolution model

**Files:**
- Modify: `eh_client/src/client.rs`
- Test: `eh_client/src/client.rs`

- [ ] **Step 1: Add failing tests for resolution parsing and archive capability**

Add tests in the existing `#[cfg(test)] mod tests` in `eh_client/src/client.rs`:

```rust
#[test]
fn eh_resolution_parses_known_values() {
    assert_eq!(EhResolution::parse("archive").unwrap(), EhResolution::Width(1280));
    assert_eq!(EhResolution::parse("resample").unwrap(), EhResolution::Width(1280));
    assert_eq!(EhResolution::parse("original").unwrap(), EhResolution::Original);
    assert_eq!(EhResolution::parse("800x").unwrap(), EhResolution::Width(800));
    assert_eq!(EhResolution::parse("1280x").unwrap(), EhResolution::Width(1280));
    assert_eq!(EhResolution::parse("2400").unwrap(), EhResolution::Width(2400));
}

#[test]
fn eh_resolution_rejects_unknown_values() {
    assert!(EhResolution::parse("").is_err());
    assert!(EhResolution::parse("abc").is_err());
    assert!(EhResolution::parse("0x").is_err());
}

#[test]
fn eh_resolution_archive_capability_is_explicit() {
    assert!(EhResolution::Width(1280).can_use_archiver());
    assert!(EhResolution::Original.can_use_archiver());
    assert!(!EhResolution::Width(800).can_use_archiver());
    assert!(!EhResolution::Width(1600).can_use_archiver());
}
```

- [ ] **Step 2: Run tests and verify they fail**

Run:

```powershell
cargo test -p eh_client eh_resolution_ --lib
```

Expected: compile failure because `EhResolution` does not exist.

- [ ] **Step 3: Implement `EhResolution`**

Add near the top of `eh_client/src/client.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EhArchiveKind {
    Resample1280,
    Original,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EhResolution {
    /// EH original archive mode.
    Original,
    /// Image width. Only 1280 is also the EH archiver resample mode.
    Width(u32),
}

impl EhResolution {
    pub fn parse(raw: &str) -> Result<Self> {
        let value = raw.trim().to_ascii_lowercase();
        if value.is_empty() {
            return Err(Error::Parse("EH resolution cannot be empty".into()));
        }
        if value == "archive" || value == "resample" {
            return Ok(Self::Width(1280));
        }
        if value == "original" || value == "org" {
            return Ok(Self::Original);
        }
        let numeric = value.trim_end_matches('x');
        let width = numeric
            .parse::<u32>()
            .map_err(|_| Error::Parse(format!("unsupported EH resolution: {raw}")))?;
        if width == 0 {
            return Err(Error::Parse(format!("unsupported EH resolution: {raw}")));
        }
        Ok(Self::Width(width))
    }

    pub fn can_use_archiver(self) -> bool {
        self.archive_kind().is_some()
    }

    fn archive_kind(self) -> Option<EhArchiveKind> {
        match self {
            Self::Original => Some(EhArchiveKind::Original),
            Self::Width(1280) => Some(EhArchiveKind::Resample1280),
            Self::Width(_) => None,
        }
    }
}
```

Also change the archiver form-data helpers so resample and original are explicit instead of overloading an empty string:

```rust
fn archive_form_data_for_kind(kind: EhArchiveKind) -> Vec<(String, String)> {
    match kind {
        EhArchiveKind::Original => vec![
            ("dlcheck".to_string(), "Download Original Archive".to_string()),
            ("hathdl_xres".to_string(), "org".to_string()),
        ],
        EhArchiveKind::Resample1280 => vec![
            ("dlcheck".to_string(), "Download Resample Archive".to_string()),
            ("hathdl_xres".to_string(), "1280".to_string()),
        ],
    }
}
```

Update `ArchiveDownloadRequest::from_archiver_key()` and `ArchiveDownloadRequest::from_archiver_form()` to take `EhArchiveKind` instead of `resolution: &str`, and update `apply_resolution_to_form_data()` into `apply_archive_kind_to_form_data()` with the same two explicit branches.

- [ ] **Step 4: Run tests and verify they pass**

Run:

```powershell
cargo test -p eh_client eh_resolution_ --lib
```

Expected: all three new tests pass.

## Task 2: Detect archiver availability and choose archive vs direct routing

**Files:**
- Modify: `eh_client/src/client.rs`
- Modify: `eh_client/src/parser.rs`
- Test: `eh_client/tests/integration.rs`

- [ ] **Step 1: Add failing integration tests for routing**

Add to `eh_client/tests/integration.rs`:

```rust
#[tokio::test]
async fn test_download_artifact_uses_archiver_for_archive_resolution() {
    let server = MockServer::start().await;
    mock_gallery_with_archiver(&server, 123456, "abcdef0123", "fedcba9876").await;
    mock_archiver_form_original_only(&server, 123456, "fedcba9876").await;
    mock_archiver_post_zip(&server, 123456, "fedcba9876").await;

    let client = logged_in_client_at(&server);
    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("archive.zip");
    client
        .download_gallery_artifact(123456, "abcdef0123", EhResolution::Original, &dest)
        .await
        .expect("original should use archiver when original form is available");
    assert!(std::fs::read(dest).unwrap().starts_with(b"PK\x03\x04"));
}

#[tokio::test]
async fn test_download_artifact_uses_archiver_resample_for_1280_aliases() {
    let server = MockServer::start().await;
    mock_gallery_with_archiver(&server, 123456, "abcdef0123", "fedcba9876").await;
    mock_archiver_form_resample(&server, 123456, "fedcba9876").await;
    mock_archiver_post_zip_with_body(
        &server,
        123456,
        "fedcba9876",
        &[BodyContains("dlcheck=Download+Resample+Archive"), BodyContains("hathdl_xres=1280")],
    )
    .await;

    let client = logged_in_client_at(&server);
    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("archive.zip");
    client
        .download_gallery_artifact(123456, "abcdef0123", EhResolution::Width(1280), &dest)
        .await
        .expect("1280x/resample should use archiver resample when available");
    assert!(std::fs::read(dest).unwrap().starts_with(b"PK\x03\x04"));
}

#[tokio::test]
async fn test_download_artifact_uses_direct_for_high_width_resolution() {
    let server = MockServer::start().await;
    mock_gallery_with_two_image_pages(&server).await;
    mock_image_page_with_src(&server, "/s/1", "/img/1_1600.jpg").await;
    mock_image_page_with_src(&server, "/s/2", "/img/2_1600.jpg").await;
    mock_image_bytes(&server, "/img/1_1600.jpg", b"image-one").await;
    mock_image_bytes(&server, "/img/2_1600.jpg", b"image-two").await;

    let client = client_at(&server);
    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("direct.zip");
    client
        .download_gallery_artifact(123456, "abcdef0123", EhResolution::Width(1600), &dest)
        .await
        .expect("high-width requests must use direct image pages");
    assert!(std::fs::read(dest).unwrap().starts_with(b"PK\x03\x04"));
}

#[tokio::test]
async fn test_download_artifact_falls_back_to_direct_when_archiver_link_missing() {
    let server = MockServer::start().await;
    mock_gallery_with_two_image_pages_without_archiver(&server).await;
    mock_image_page_with_src(&server, "/s/1", "/img/1_current.jpg").await;
    mock_image_page_with_src(&server, "/s/2", "/img/2_current.jpg").await;
    mock_image_bytes(&server, "/img/1_current.jpg", b"image-one").await;
    mock_image_bytes(&server, "/img/2_current.jpg", b"image-two").await;

    let client = logged_in_client_at(&server);
    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("direct.zip");
    client
        .download_gallery_artifact(123456, "abcdef0123", EhResolution::Original, &dest)
        .await
        .expect("missing archiver link should fall back to direct gallery download");
    assert!(std::fs::read(dest).unwrap().starts_with(b"PK\x03\x04"));
}

#[tokio::test]
async fn test_download_artifact_uses_direct_for_800_width_resolution() {
    let server = MockServer::start().await;
    mock_gallery_with_two_image_pages(&server).await;
    mock_image_page_with_src(&server, "/s/1", "/img/1_800.jpg").await;
    mock_image_page_with_src(&server, "/s/2", "/img/2_800.jpg").await;
    mock_image_bytes(&server, "/img/1_800.jpg", b"image-one").await;
    mock_image_bytes(&server, "/img/2_800.jpg", b"image-two").await;

    let client = client_at(&server);
    let temp_dir = tempfile::tempdir().unwrap();
    let dest = temp_dir.path().join("direct.zip");
    client
        .download_gallery_artifact(123456, "abcdef0123", EhResolution::Width(800), &dest)
        .await
        .expect("800x must use direct image pages, not archiver");
    assert!(std::fs::read(dest).unwrap().starts_with(b"PK\x03\x04"));
}
```

Create local helper functions in the same test file using existing wiremock patterns. The archiver tests must use `logged_in_client_at(&server)`, which builds an `EhClient` with all required cookies so `is_logged_in()` is true. The helpers must return gallery HTML compatible with `parse_archiver_url()` and `parse_image_page_urls()`.

- [ ] **Step 2: Run tests and verify they fail**

Run:

```powershell
cargo test -p eh_client test_download_artifact_ --test integration
```

Expected: compile failure because `download_gallery_artifact()` is missing.

- [ ] **Step 3: Implement routing API**

Add to `impl EhClient` in `eh_client/src/client.rs`:

```rust
pub async fn download_gallery_artifact(
    &self,
    gid: u64,
    token: &str,
    resolution: EhResolution,
    dest: &Path,
) -> Result<u64> {
    // Only 1280px resample and original can use EH archiver.  All other
    // explicit widths must use direct gallery image-page downloads.
    if self.is_logged_in() {
        if let Some(kind) = resolution.archive_kind() {
            if let Some(request) = self.prepare_archive_download_for_kind(gid, token, kind).await? {
                return self.download_archive_with_request(&request, dest).await;
            }
        }
    }

    self.download_gallery_images_with_resolution(gid, token, resolution, dest)
        .await
}
```

Add `prepare_archive_download_for_kind()` next to the existing `prepare_archive_download()`:

```rust
pub async fn prepare_archive_download_for_kind(
    &self,
    gid: u64,
    token: &str,
    kind: EhArchiveKind,
) -> Result<Option<ArchiveDownloadRequest>> {
    let Some((archiver_gid, archiver_token, archiver_html)) =
        self.fetch_optional_archiver_page(gid, token).await?
    else {
        return Ok(None);
    };

    let form = parser::parse_archiver_form(&archiver_html);
    if let Some(ref form) = form {
        if !form.supports_archive_kind(kind) {
            return Ok(None);
        }
    }

    if let Some(archiver_key) = parser::parse_archiver_key(&archiver_html) {
        // Legacy key-only pages do not expose enough form detail. Preserve the
        // old behavior by allowing both explicit archive kinds when a key exists.
        return Ok(Some(ArchiveDownloadRequest::from_archiver_key(
            &self.base_url,
            archiver_gid,
            &archiver_token,
            &archiver_key,
            kind,
        )));
    }

    let Some(form) = form else {
        return Ok(None);
    };
    Ok(Some(ArchiveDownloadRequest::from_archiver_form(
        &self.base_url,
        form,
        kind,
    )))
}
```

Add `fetch_optional_archiver_page()` beside `fetch_archiver_page()` and use it only for resolution routing fallback:

```rust
async fn fetch_optional_archiver_page(
    &self,
    gid: u64,
    token: &str,
) -> Result<Option<(u64, String, String)>> {
    let gallery_url = format!("{}/g/{}/{}/", self.base_url, gid, token);
    let resp = self
        .http
        .get(&gallery_url)
        .header(COOKIE, self.cookies.to_header())
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::Api {
            message: format!("gallery page returned {}", status),
            status: status.as_u16(),
        });
    }
    let gallery_html = resp.text().await?;

    let Some((archiver_gid, archiver_token)) = parser::parse_archiver_url(&gallery_html) else {
        return Ok(None);
    };

    let archiver_page_url = format!(
        "{}/archiver.php?gid={}&token={}",
        self.base_url, archiver_gid, archiver_token
    );
    let resp = self
        .http
        .get(&archiver_page_url)
        .header(COOKIE, self.cookies.to_header())
        .send()
        .await?;
    let status = resp.status();
    if status.as_u16() == 403 || status.as_u16() == 404 || status.as_u16() == 410 {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(Error::Api {
            message: format!("archiver.php returned {}", status),
            status: status.as_u16(),
        });
    }
    Ok(Some((archiver_gid, archiver_token, resp.text().await?)))
}
```

This fallback contract is intentionally narrow: missing archiver link, unsupported archiver form, and archiver `403/404/410` fall back to direct gallery downloads; gallery fetch failures, network errors, and archiver `5xx` still fail so transient site problems do not silently change delivery mode.

Define `ArchiverForm::supports_archive_kind(kind)` in `eh_client/src/parser.rs`:

```rust
impl ArchiverForm {
    pub fn supports_archive_kind(&self, kind: crate::client::EhArchiveKind) -> bool {
        match kind {
            crate::client::EhArchiveKind::Original => self.fields.iter().any(|(name, value)| {
                (name == "dltype" && value == "org")
                    || (name == "dlcheck" && value.contains("Original"))
            }),
            crate::client::EhArchiveKind::Resample1280 => self.fields.iter().any(|(name, value)| {
                (name == "hathdl_xres" && value == "1280")
                    || (name == "dlcheck" && value.contains("Resample"))
            }),
        }
    }
}
```

Add parser unit tests for an original-only form, a resample form, and a form that supports both.

Rename the existing `download_gallery_images()` implementation body into:

```rust
pub async fn download_gallery_images_with_resolution(
    &self,
    gid: u64,
    token: &str,
    resolution: EhResolution,
    dest: &Path,
) -> Result<u64> {
    // Start with the current implementation, using parse_image_src() as the first
    // available direct image URL. Task 3 will add alternate-size fallback.
}
```

Keep the old API as a compatibility wrapper:

```rust
pub async fn download_gallery_images(&self, gid: u64, token: &str, dest: &Path) -> Result<u64> {
    self.download_gallery_images_with_resolution(gid, token, EhResolution::Original, dest)
        .await
}
```

This preserves the current legacy behavior: callers of `download_gallery_images()`
without an explicit resolution keep using the currently served `<img id="img">`
image. Explicit width requests go through `download_gallery_images_with_resolution()`
and must follow the downward-only candidate rules in Task 3.

- [ ] **Step 4: Run routing tests**

Run:

```powershell
cargo test -p eh_client test_download_artifact_ --test integration
```

Expected: both tests pass.

## Task 3: Add direct image fallback ordering for high resolutions

**Files:**
- Modify: `eh_client/src/parser.rs`
- Modify: `eh_client/src/client.rs`
- Test: `eh_client/src/parser.rs`
- Test: `eh_client/tests/integration.rs`

- [ ] **Step 1: Add parser tests for image alternatives**

Add tests in `eh_client/src/parser.rs`:

```rust
#[test]
fn test_parse_image_candidates_prefers_requested_width_then_lower() {
    let html = r#"
      <a href="https://host/full_2400.jpg">2400x</a>
      <a href="https://host/full_1600.jpg">1600x</a>
      <a href="https://host/full_1280.jpg">1280x</a>
      <img id="img" src="https://host/current_980.jpg" />
    "#;
    let urls = parse_image_candidates(html, 1600);
    assert_eq!(urls[0], "https://host/full_1600.jpg");
    assert_eq!(urls[1], "https://host/full_1280.jpg");
    assert!(!urls.contains(&"https://host/full_2400.jpg".to_string()));
    assert!(!urls.contains(&"https://host/current_980.jpg".to_string()));
}

#[test]
fn test_parse_image_candidates_returns_empty_when_no_labeled_lower_candidate_exists() {
    let html = r#"
      <a href="https://host/full_2400.jpg">2400x</a>
      <img id="img" src="https://host/current_980.jpg" />
    "#;
    let urls = parse_image_candidates(html, 1600);
    assert!(urls.is_empty());
}

#[test]
fn test_parse_image_candidates_for_original_can_use_current_image() {
    let html = r#"
      <img id="img" src="https://host/current_original.jpg" />
    "#;
    let urls = parse_image_candidates_for_original(html);
    assert_eq!(urls, vec!["https://host/current_original.jpg".to_string()]);
}
```

- [ ] **Step 2: Run parser test and verify it fails**

Run:

```powershell
cargo test -p eh_client parser::tests::test_parse_image_candidates_prefers_requested_width_then_lower
```

Expected: compile failure because `parse_image_candidates()` is missing.

- [ ] **Step 3: Implement `parse_image_candidates()`**

Add to `eh_client/src/parser.rs`:

```rust
pub fn parse_image_candidates(html: &str, requested_width: u32) -> Vec<String> {
    let mut candidates: Vec<(u32, String)> = Vec::new();

    let link_re = Regex::new(r#"(?is)<a\b[^>]*href=[\"'](https?://[^\"']+)[\"'][^>]*>([^<]+)</a>"#)
        .expect("invalid image candidate regex");
    for cap in link_re.captures_iter(html) {
        let url = cap.get(1).unwrap().as_str().to_string();
        let label = cap.get(2).unwrap().as_str().to_ascii_lowercase();
        if let Some(width) = label
            .trim()
            .trim_end_matches('x')
            .parse::<u32>()
            .ok()
        {
            candidates.push((width, url));
        }
    }

    candidates.sort_by_key(|(width, _)| {
        if *width <= requested_width {
            (0, requested_width - *width)
        } else {
            (1, *width - requested_width)
        }
    });

    let mut urls: Vec<String> = candidates
        .into_iter()
        .filter(|(width, _)| *width <= requested_width)
        .map(|(_, url)| url)
        .collect();
    urls
}

pub fn parse_image_candidates_for_original(html: &str) -> Vec<String> {
    parse_image_src(html).into_iter().collect()
}
```

This parser intentionally accepts simple EH-shaped size links first; if live HTML needs a different selector, add a fixture and extend the parser. Explicit-width requests must never try a labeled width above the requested width and must not use an unlabeled current `<img id="img">` because its width is unknown. `Original` direct fallback may use the current image because it intentionally asks for the largest currently served image.

- [ ] **Step 4: Use candidates in direct downloads**

In `download_gallery_images_with_resolution()`, when handling each image page:

```rust
let image_candidates = match resolution {
    EhResolution::Original => parser::parse_image_candidates_for_original(&img_html),
    EhResolution::Width(width) => parser::parse_image_candidates(&img_html, width),
};

let mut last_error = None;
for image_url in image_candidates {
    match self.http.get(&image_url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let img_bytes = resp.bytes().await?;
            // write current image and break
        }
        Ok(resp) => last_error = Some(Error::Api { message: format!("image returned {}", resp.status()), status: resp.status().as_u16() }),
        Err(e) => last_error = Some(Error::Http(e)),
    }
}
```

Preserve the existing ZIP entry naming and failure behavior: if all candidates fail for one page, the whole direct download fails with the last error.

- [ ] **Step 5: Add integration fallback test**

Add a test where the requested `2400x` candidate returns 404 but `1600x` returns image bytes. Assert the resulting ZIP contains the `1600x` bytes and the 404 does not abort until lower fallback succeeds.

Run:

```powershell
cargo test -p eh_client test_direct_gallery_download_falls_back_to_lower_resolution --test integration
```

Expected: pass.

## Task 4: Add Telegraph resample configuration and image transform helper

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/config.rs`
- Modify: `src/scheduler/eh_engine.rs`
- Test: `src/scheduler/eh_engine.rs`

- [ ] **Step 1: Add failing tests for resample config parsing**

Add tests near existing EH config/scheduler tests:

```rust
#[test]
fn eh_config_telegraph_resample_parses_boolean_and_width() {
    assert_eq!(TelegraphResampleSetting::Bool(false).target_width().unwrap(), None);
    assert_eq!(TelegraphResampleSetting::Bool(true).target_width().unwrap(), Some(1280));
    assert_eq!(TelegraphResampleSetting::String("on".into()).target_width().unwrap(), Some(1280));
    assert_eq!(TelegraphResampleSetting::String("980x".into()).target_width().unwrap(), Some(980));
    assert_eq!(TelegraphResampleSetting::Number(1600).target_width().unwrap(), Some(1600));
    assert!(TelegraphResampleSetting::String("abc".into()).target_width().is_err());
}

#[test]
fn eh_config_telegraph_resample_deserializes_from_toml_shapes() {
    fn parse_eh_toml(input: &str) -> EhentaiConfig {
        config::Config::builder()
            .add_source(config::File::from_str(input, config::FileFormat::Toml))
            .build()
            .unwrap()
            .try_deserialize()
            .unwrap()
    }

    let bool_cfg = parse_eh_toml("telegraph_resample = true");
    assert_eq!(bool_cfg.telegraph_resample_target_width().unwrap(), Some(1280));

    let number_cfg = parse_eh_toml("telegraph_resample = 1600");
    assert_eq!(number_cfg.telegraph_resample_target_width().unwrap(), Some(1600));

    let string_cfg = parse_eh_toml("telegraph_resample = \"980x\"");
    assert_eq!(string_cfg.telegraph_resample_target_width().unwrap(), Some(980));

    let disabled_cfg = parse_eh_toml("telegraph_resample = false");
    assert_eq!(disabled_cfg.telegraph_resample_target_width().unwrap(), None);
}
```

- [ ] **Step 2: Add config fields**

Add to `EhentaiConfig` in `src/config.rs`:

```rust
/// Resolution for Telegraph uploads when telegraph_resample is disabled.
#[serde(default = "default_eh_telegraph_resolution")]
pub telegraph_resolution: String,
/// Optional Telegraph image downsample target. Accepts true for 1280px,
/// or an explicit width like "980x" / 980. When enabled, Telegraph downloads
/// use the normal source resolution and the upload worker converts only
/// non-JPEG images to JPEG and downscales images wider than the target width.
/// Smaller images are never upscaled.
#[serde(default)]
pub telegraph_resample: Option<TelegraphResampleSetting>,
```

Add the setting type and default helper:

```rust
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
pub enum TelegraphResampleSetting {
    Bool(bool),
    Number(u32),
    String(String),
}

impl TelegraphResampleSetting {
    pub fn target_width(&self) -> Result<Option<u32>> {
        let width = match self {
            Self::Bool(false) => return Ok(None),
            Self::Bool(true) => 1280,
            Self::Number(width) => *width,
            Self::String(raw) => {
                let value = raw.trim().to_ascii_lowercase();
                if value.is_empty() || value == "false" || value == "off" || value == "0" {
                    return Ok(None);
                }
                if value == "true" || value == "on" || value == "1" {
                    1280
                } else {
                    value
                        .trim_end_matches('x')
                        .parse::<u32>()
                        .map_err(|_| anyhow::anyhow!("invalid telegraph_resample value: {raw}"))?
                }
            }
        };
        if width == 0 {
            anyhow::bail!("invalid telegraph_resample width: 0");
        }
        Ok(Some(width))
    }
}

fn default_eh_telegraph_resolution() -> String {
    "1280x".to_string()
}
```

Add helper methods on `EhentaiConfig`:

```rust
impl EhentaiConfig {
    pub fn telegraph_resample_target_width(&self) -> Result<Option<u32>> {
        self.telegraph_resample
            .as_ref()
            .map(TelegraphResampleSetting::target_width)
            .transpose()
            .map(Option::flatten)
    }
}
```

- [ ] **Step 3: Ensure image crate decodes uploadable formats**

Check root `Cargo.toml`. If `image` does not include all uploadable formats, update it to:

```toml
image = { version = "0.25.10", default-features = false, features = ["png", "jpeg", "gif", "webp"] }
```

If the crate already has these features, leave it unchanged.

- [ ] **Step 4: Implement resample helper tests**

Add tests in `src/scheduler/eh_engine.rs`:

```rust
#[test]
fn telegraph_resample_converts_png_to_jpeg() {
    let png = make_test_png(1600, 900);
    let output = maybe_resample_for_telegraph("page.png", png, Some(1280)).unwrap();
    assert_eq!(output.filename, "page.jpg");
    assert!(output.data.starts_with(&[0xFF, 0xD8]));
    let decoded = image::load_from_memory(&output.data).unwrap();
    assert_eq!(decoded.width(), 1280);
}

#[test]
fn telegraph_resample_converts_small_png_without_upscaling() {
    let png = make_test_png(800, 600);
    let output = maybe_resample_for_telegraph("page.png", png, Some(1280)).unwrap();
    assert_eq!(output.filename, "page.jpg");
    let decoded = image::load_from_memory(&output.data).unwrap();
    assert_eq!(decoded.width(), 800);
}

#[test]
fn telegraph_resample_keeps_small_jpeg_unchanged() {
    let jpg = make_test_jpeg(800, 600);
    let output = maybe_resample_for_telegraph("page.jpg", jpg.clone(), Some(1280)).unwrap();
    assert_eq!(output.filename, "page.jpg");
    assert_eq!(output.data, jpg);
}
```

- [ ] **Step 5: Implement `maybe_resample_for_telegraph()`**

Add near `ZipImageData` in `src/scheduler/eh_engine.rs`:

```rust
fn maybe_resample_for_telegraph(
    filename: &str,
    data: Vec<u8>,
    target_width: Option<u32>,
) -> Result<ZipImageData> {
    let Some(target_width) = target_width else {
        return Ok(ZipImageData { filename: filename.to_string(), data });
    };

    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_jpeg = matches!(ext.as_str(), "jpg" | "jpeg");

    let image = image::load_from_memory(&data)
        .with_context(|| format!("Failed to decode image for Telegraph resample: {filename}"))?;
    if is_jpeg && image.width() <= target_width {
        return Ok(ZipImageData { filename: filename.to_string(), data });
    }

    let resized = if image.width() > target_width {
        image.resize(target_width, u32::MAX, image::imageops::FilterType::Lanczos3)
    } else {
        image
    };
    use image::ImageEncoder;

    let mut out = Vec::new();
    let rgb = resized.to_rgb8();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
    encoder
        .write_image(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .context("Failed to encode Telegraph JPEG")?;

    let stem = std::path::Path::new(filename)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("image");
    Ok(ZipImageData { filename: format!("{stem}.jpg"), data: out })
}
```

- [ ] **Step 6: Apply helper in upload worker**

In `EhUploadWorker::process()`, parse once before spawning the reader:

```rust
let telegraph_resample_width = self.config.telegraph_resample_target_width()?;
```

Inside the blocking ZIP reader, after reading `filename` and `data`, call:

```rust
let image = maybe_resample_for_telegraph(&filename, data, telegraph_resample_width)?;
image_tx.blocking_send(image)
```

This keeps memory bounded to one decoded image plus one encoded JPEG.

## Task 5: Update scheduler resolution selection

**Files:**
- Modify: `src/config.rs`
- Modify: `src/scheduler/eh_engine.rs`
- Test: `src/scheduler/eh_engine.rs`

- [ ] **Step 1: Add config field and tests**

Add to `EhentaiConfig` in `src/config.rs`:

```rust
/// Resolution for all Telegraph uploads when telegraph_resample is disabled.
#[serde(default = "default_eh_telegraph_resolution")]
pub telegraph_resolution: String,
```

Add the default helper and update `Default for EhentaiConfig`:

```rust
fn default_eh_telegraph_resolution() -> String {
    "1280x".to_string()
}
```

Add a method:

```rust
impl EhentaiConfig {
    pub fn resolution_for_entry(&self, source: &str, telegraph: bool) -> Result<&str> {
        if telegraph && self.telegraph_resample_target_width()?.is_none() {
            return Ok(&self.telegraph_resolution);
        }
        Ok(if source == crate::db::repo::eh_download_queue::SOURCE_DIRECT {
            &self.download_resolution
        } else {
            &self.subscription_resolution
        })
    }
}
```

If `src/config.rs` must not depend on DB constants, use literal comparison `source == "direct"` and document it.

- [ ] **Step 2: Update scheduler call site**

Replace the resolution selection block in `EhDownloadWorker::process()` with:

```rust
let resolution = EhResolution::parse(self.config.resolution_for_entry(&entry.source, entry.telegraph)?)?;
let file_size = self
    .client
    .download_gallery_artifact(gid, token, resolution, &zip_path)
    .await
    .context("Failed to download gallery artifact")?;
```

This removes the separate logged-in branch because the client owns archive-vs-direct routing.

- [ ] **Step 3: Add scheduler selection tests**

Add unit tests near existing EH integration tests:

```rust
#[test]
fn eh_config_resolution_for_entry_prefers_telegraph_resolution() {
    let mut config = make_config();
    config.subscription_resolution = "original".to_string();
    config.download_resolution = "2400x".to_string();
    config.telegraph_resolution = "1280x".to_string();
    assert_eq!(config.resolution_for_entry("subscription", true).unwrap(), "1280x");
    assert_eq!(config.resolution_for_entry("direct", true).unwrap(), "1280x");
    assert_eq!(config.resolution_for_entry("direct", false).unwrap(), "2400x");
    assert_eq!(config.resolution_for_entry("subscription", false).unwrap(), "original");
}

#[test]
fn eh_config_resolution_for_entry_skips_telegraph_resolution_when_resample_enabled() {
    let mut config = make_config();
    config.subscription_resolution = "original".to_string();
    config.download_resolution = "2400x".to_string();
    config.telegraph_resolution = "1280x".to_string();
    config.telegraph_resample = Some(TelegraphResampleSetting::String("980x".to_string()));
    assert_eq!(config.resolution_for_entry("subscription", true).unwrap(), "original");
    assert_eq!(config.resolution_for_entry("direct", true).unwrap(), "2400x");
}
```

Run:

```powershell
cargo test -p pixivbot eh_config_resolution_for_entry_prefers_telegraph_resolution
cargo test -p pixivbot eh_config_resolution_for_entry_skips_telegraph_resolution_when_resample_enabled
```

Expected: pass.

## Task 6: Retire stale string-based archive width semantics

**Files:**
- Modify: `eh_client/src/client.rs`
- Modify: `eh_client/tests/integration.rs`

- [ ] **Step 1: Add failing tests for legacy archive API rejection**

Add to `eh_client/tests/integration.rs`:

```rust
#[tokio::test]
async fn test_prepare_archive_download_rejects_legacy_non_archive_width() {
    let server = MockServer::start().await;
    mock_gallery_with_archiver(&server, 123456, "abcdef0123", "fedcba9876").await;
    mock_archiver_form_resample(&server, 123456, "fedcba9876").await;

    let client = logged_in_client_at(&server);
    let err = client
        .prepare_archive_download(123456, "abcdef0123", "780x")
        .await
        .expect_err("780x must not be accepted as an archive resolution");
    assert!(err.to_string().contains("unsupported archive resolution"));
}
```

- [ ] **Step 2: Run the test and verify it fails**

Run:

```powershell
cargo test -p eh_client test_prepare_archive_download_rejects_legacy_non_archive_width --test integration
```

Expected: FAIL because `780x` is currently treated as a resample archive form value.

- [ ] **Step 3: Narrow the compatibility API**

Keep `prepare_archive_download(gid, token, resolution: &str)` and `download_archive(gid, token, archiver_key, resolution, dest)` for compatibility, but make them parse the string and reject non-archive-capable values:

```rust
fn parse_archive_kind_arg(resolution: &str) -> Result<EhArchiveKind> {
    let parsed = EhResolution::parse(resolution)?;
    parsed
        .archive_kind()
        .ok_or_else(|| Error::Parse(format!("unsupported archive resolution: {resolution}")))
}
```

Use it in `prepare_archive_download()`:

```rust
pub async fn prepare_archive_download(
    &self,
    gid: u64,
    token: &str,
    resolution: &str,
) -> Result<ArchiveDownloadRequest> {
    let kind = parse_archive_kind_arg(resolution)?;
    self.prepare_archive_download_for_kind(gid, token, kind)
        .await?
        .ok_or_else(|| Error::Parse("archiver download form not found in archiver.php response".into()))
}
```

Use it in `download_archive()` before building `ArchiveDownloadRequest::from_archiver_key(...)`. Update old tests that passed `"780x"` to use `"resample"` or `"1280x"` when they are testing archive behavior.

Also update the `download_archive()` doc comment in `eh_client/src/client.rs` so it
no longer advertises `"780x"`, `"980x"`, `"1600x"`, or `"2400x"` as archive
resolutions. The comment should state that the compatibility archive API accepts
only `"resample"`/`"archive"`/`"1280x"` and `"original"`; other widths are
handled by `download_gallery_artifact()` via direct gallery downloads.

Update the `test_build_archiver_url` unit test in `eh_client/src/client.rs` so it
does not encode stale `780x` semantics. Use an archive-key-like value such as
`"470592--63bbddc729b849100ec24ab920ffdb84b6542b23"` or a neutral placeholder
`"archive-key"` in the `or=` assertion; this test should only prove URL assembly,
not claim `780x` is a valid archive request.

- [ ] **Step 4: Run archive tests**

Run:

```powershell
cargo test -p eh_client test_prepare_archive_download_rejects_legacy_non_archive_width --test integration
cargo test -p eh_client test_download_archive --test integration
```

Expected: all pass.

## Task 7: Update user-facing config docs

**Files:**
- Modify: `config.toml.example`
- Modify: `src/config.rs`

- [ ] **Step 1: Replace old resolution wording**

Update `config.toml.example` EH section to say:

```toml
# Resolution selection:
#   "resample" / "archive" / "1280x" - 1280px resample; only these can use EH archiver resample
#   "original"                       - only this can use EH original archive
#   "1600x", "2400x", ...            - direct image-page download only; fall back downward when unavailable
# subscription_resolution = "resample"
# download_resolution = "resample"
# telegraph_resolution = "1280x"  # used for every telegraph=true row when telegraph_resample is unset
# telegraph_resample = true        # optional; true=1280px JPG, or set e.g. "980x"
```

Document `telegraph_resample` behavior: when set, Telegraph entries download using `subscription_resolution` or `download_resolution` as appropriate, then upload worker converts non-JPEG images to JPEG and downscales images wider than the target width. JPEGs at or below the target width are uploaded unchanged. Non-JPEG images below the target width are converted to JPEG at their original dimensions; the bot never upscales images.

Update `src/config.rs` comments for the same semantics.

- [ ] **Step 2: Run markdown/whitespace check**

Run:

```powershell
git diff --check -- config.toml.example src/config.rs
```

Expected: no output.

## Task 8: Focused verification

**Files:** all modified files.

- [ ] **Step 1: Run EH client focused tests**

```powershell
cargo test -p eh_client eh_resolution_ --lib
cargo test -p eh_client parser::tests::test_parse_image_candidates_prefers_requested_width_then_lower
cargo test -p eh_client test_download_artifact_ --test integration
cargo test -p eh_client test_direct_gallery_download_falls_back_to_lower_resolution --test integration
```

Expected: all pass.

- [ ] **Step 2: Run scheduler/config tests**

```powershell
cargo test -p pixivbot eh_config_resolution_for_entry_prefers_telegraph_resolution
cargo test -p pixivbot eh_config_resolution_for_entry_skips_telegraph_resolution_when_resample_enabled
cargo test -p pixivbot test_download_worker_downloads_archive
cargo test -p pixivbot telegraph_resample_
```

Expected: all pass; update existing mocks if they now route through `download_gallery_artifact()`.

- [ ] **Step 3: Run repo checks**

```powershell
cargo fmt --all -- --check
git diff --check
cargo test -p eh_client
cargo test -p pixivbot test_download_worker
$env:RUSTFLAGS = "-Dwarnings"; cargo clippy -p eh_client -p pixivbot --all-targets -- -D warnings
cargo check -p eh_client -p pixivbot --all-targets
```

Expected: all pass.

## Plan self-review

- Spec coverage: Covers archive-vs-direct routing, typed resolution parsing, downward direct fallback, scheduler config selection, Telegraph resample/downsample behavior including no-upscale semantics, docs, and tests.
- Placeholder scan: No TBD/TODO placeholders remain. The plan intentionally includes a parser assumption for EH image size links and instructs adding fixtures if live HTML differs.
- Type consistency: Uses `EhResolution`, `EhArchiveKind`, `TelegraphResampleSetting`, `telegraph_resample_target_width`, `download_gallery_artifact`, `download_gallery_images_with_resolution`, `parse_image_candidates`, and `resolution_for_entry` consistently across tasks.

## Execution handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-07-eh-resolution-routing.md`.

Two execution options:

1. **Subagent-Driven (recommended)** - dispatch focused implementation/review tasks.
2. **Inline Execution** - execute this plan in the current session with checkpoints.
