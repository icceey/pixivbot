# E-Hentai/ExHentai Gallery Subscription Implementation Plan

> **For agentic workers:** Use the subagent-driven-development skill to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add e-hentai/exhentai gallery subscription support — subscribe via search syntax, poll for new galleries, download archive ZIPs at configured resolution, send to Telegram chats, with optional Telegraph upload and rate-limited persistent download queue.

**Architecture:** New `eh_client` workspace crate (HTTP client + HTML parser + Telegraph client). Two scheduler components: `EhEngine` (search-only, enqueues downloads) and `EhDownloadProcessor` (drains queue with rate limiting). New DB migration adds `eh_filter` column + `eh_download_queue` table. New bot commands `/esub`, `/eunsub`, `/elist`, `/edl`.

**Tech Stack:** Rust 1.94, reqwest (rustls), regex, serde/serde_json, zip, sea-orm (SQLite), teloxide, tokio, chrono, anyhow, tracing

---

## File Structure

```
eh_client/                          ← NEW workspace crate
  Cargo.toml
  src/
    lib.rs                          ← module exports
    error.rs                        ← Error/Result types
    models.rs                       ← EhGallery, EhGalleryRef, EhCategory, EhCookies
    parser.rs                       ← HTML parsing (search results, archiver redirect)
    client.rs                       ← EhClient: search, metadata, archive download
    telegraph.rs                    ← TelegraphClient: upload, create_page

migration/src/
  m20260626_000000_add_ehentai.rs   ← NEW: add eh_filter col + eh_download_queue table
  lib.rs                            ← MODIFY: register new migration

src/
  config.rs                         ← MODIFY: add EhentaiConfig
  db/
    entities/
      mod.rs                        ← MODIFY: add eh_download_queue
      eh_download_queue.rs          ← NEW: SeaORM entity
    types/
      mod.rs                        ← MODIFY: add eh_filter, eh_task_key
      task_type.rs                   ← MODIFY: add Ehentai variant
      state.rs                      ← MODIFY: add EhTag variant
      eh_filter.rs                  ← NEW: EhFilter struct
      eh_task_key.rs                ← NEW: EhTaskKey task value encoding
    repo/
      mod.rs                        ← MODIFY: add eh_download_queue
      subscriptions.rs              ← MODIFY: add upsert_eh_subscription
      eh_download_queue.rs          ← NEW: queue CRUD + rate-limit accounting
  scheduler/
    mod.rs                          ← MODIFY: add eh_engine, eh_download_processor
    eh_engine.rs                    ← NEW: search-only engine
    eh_download_processor.rs        ← NEW: download queue drainer
  bot/
    commands.rs                     ← MODIFY: add ESub/EUnsub/EList/EDl
    handler.rs                      ← MODIFY: add eh_client field + dispatch
    handlers/
      subscription/
        mod.rs                      ← MODIFY: add ehentai, eh_download modules
        ehentai.rs                  ← NEW: handle_esub/handle_eunsub/handle_elist
        eh_download.rs              ← NEW: handle_edl
    notifier.rs                     ← MODIFY: add notify_with_document, notify_with_text
    notifier/
      document.rs                  ← NEW: document + text send methods
    mod.rs                          ← MODIFY: add has_ehentai, pass eh_client
  utils/
    caption.rs                      ← MODIFY: add build_eh_caption
  main.rs                           ← MODIFY: build EhClient, spawn engines, pass to bot

config.toml.example                 ← MODIFY: add [ehentai] section
Cargo.toml                           ← MODIFY: add eh_client member + dep
```

---

## Task 1: Create `eh_client` Crate Scaffolding

**Files:**
- Create: `eh_client/Cargo.toml`
- Create: `eh_client/src/lib.rs`
- Create: `eh_client/src/error.rs`
- Create: `eh_client/src/models.rs`
- Modify: `Cargo.toml` (root)

- [ ] **Step 1: Create `eh_client/Cargo.toml`**

```toml
[package]
name = "eh_client"
version.workspace = true
edition.workspace = true
authors.workspace = true
license.workspace = true
rust-version = "1.94"

[dependencies]
chrono = { version = "0.4.44", features = ["serde"] }
regex = "1.11.1"
reqwest = { version = "0.12.28", default-features = false, features = ["json", "rustls-tls", "multipart"] }
serde = { version = "1.0.228", features = ["derive"] }
serde_json = "1.0.150"
tracing = "0.1.44"
urlencoding = "2.1.3"
```

- [ ] **Step 2: Create `eh_client/src/error.rs`**

```rust
use std::fmt;

#[derive(Debug)]
pub enum Error {
    Http(reqwest::Error),
    Json(serde_json::Error),
    Api { message: String, status: u16 },
    Parse(String),
    Io(std::io::Error),
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Http(e) => write!(f, "HTTP error: {}", e),
            Error::Json(e) => write!(f, "JSON parse error: {}", e),
            Error::Api { message, status } => write!(f, "API error ({}): {}", status, message),
            Error::Parse(msg) => write!(f, "Parse error: {}", msg),
            Error::Io(e) => write!(f, "IO error: {}", e),
            Error::Other(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for Error {}

impl From<reqwest::Error> for Error {
    fn from(err: reqwest::Error) -> Self {
        Error::Http(err)
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Error::Json(err)
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io(err)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
```

- [ ] **Step 3: Create `eh_client/src/models.rs`**

```rust
use serde::{Deserialize, Serialize};

/// Cookies for e-hentai/exhentai authentication.
#[derive(Debug, Clone, Default)]
pub struct EhCookies {
    pub ipb_member_id: Option<String>,
    pub ipb_pass_hash: Option<String>,
    pub igneous: Option<String>,
    pub nw: bool,
}

impl EhCookies {
    /// Build a Cookie header value string.
    pub fn to_header(&self) -> String {
        let mut parts = Vec::new();
        if let Some(ref id) = self.ipb_member_id {
            parts.push(format!("ipb_member_id={id}"));
        }
        if let Some(ref hash) = self.ipb_pass_hash {
            parts.push(format!("ipb_pass_hash={hash}"));
        }
        if let Some(ref ig) = self.igneous {
            parts.push(format!("igneous={ig}"));
        }
        if self.nw {
            parts.push("nw=1".to_string());
        }
        parts.join("; ")
    }

    /// True if this is an exhentai-capable cookie set (all three required).
    pub fn is_exhentai_capable(&self) -> bool {
        self.ipb_member_id.is_some()
            && self.ipb_pass_hash.is_some()
            && self.igneous.is_some()
    }
}

/// A gallery reference parsed from search HTML results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EhGalleryRef {
    pub gid: u64,
    pub token: String,
    pub title: String,
    pub url: String,
    pub posted_ts: i64,
}

/// Full gallery metadata from the api.php JSON endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EhGallery {
    pub gid: u64,
    pub token: String,
    pub title: String,
    pub title_jpn: Option<String>,
    pub category: String,
    pub thumb: String,
    pub uploader: String,
    pub posted: i64,
    pub filecount: u32,
    pub filesize: u64,
    pub expunged: bool,
    pub rating: f64,
    pub tags: Vec<String>,
}

/// E-hentai gallery categories with their bitmask values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EhCategory {
    Doujinshi = 1,
    Manga = 2,
    ArtistCG = 4,
    GameCG = 8,
    Western = 16,
    NonH = 32,
    ImageSet = 64,
    Cosplay = 128,
    AsianPorn = 256,
    Misc = 512,
}

impl EhCategory {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "doujinshi" => Some(Self::Doujinshi),
            "manga" => Some(Self::Manga),
            "artistcg" | "artist cg" | "artist_cg" => Some(Self::ArtistCG),
            "gamecg" | "game cg" | "game_cg" => Some(Self::GameCG),
            "western" => Some(Self::Western),
            "nonh" | "non-h" | "non_h" => Some(Self::NonH),
            "imageset" | "image set" | "image_set" => Some(Self::ImageSet),
            "cosplay" => Some(Self::Cosplay),
            "asianporn" | "asian porn" | "asian_porn" => Some(Self::AsianPorn),
            "misc" => Some(Self::Misc),
            _ => None,
        }
    }

    /// Parse a comma-separated list of category names into a bitmask.
    pub fn bitmask_from_str(s: &str) -> u32 {
        s.split(',')
            .filter_map(|c| Self::from_str(c.trim()))
            .map(|c| c as u32)
            .sum()
    }
}

/// Raw API response structures (internal).
#[derive(Debug, Deserialize)]
pub(crate) struct RawApiResponse {
    pub gmetadata: Vec<RawGalleryMeta>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) struct RawGalleryMeta {
    pub gid: u64,
    pub token: String,
    pub title: String,
    #[serde(default)]
    pub title_jpn: Option<String>,
    pub category: String,
    pub thumb: String,
    pub uploader: String,
    pub posted: String,
    pub filecount: String,
    pub filesize: u64,
    #[serde(default)]
    pub expunged: bool,
    pub rating: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl RawGalleryMeta {
    pub fn into_gallery(self) -> EhGallery {
        let posted = self.posted.parse::<i64>().unwrap_or(0);
        let filecount = self.filecount.parse::<u32>().unwrap_or(0);
        let rating = self.rating.parse::<f64>().unwrap_or(0.0);
        EhGallery {
            gid: self.gid,
            token: self.token,
            title: self.title,
            title_jpn: self.title_jpn,
            category: self.category,
            thumb: self.thumb,
            uploader: self.uploader,
            posted,
            filecount,
            filesize: self.filesize,
            expunged: self.expunged,
            rating,
            tags: self.tags,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cookie_header() {
        let cookies = EhCookies {
            ipb_member_id: Some("12345".into()),
            ipb_pass_hash: Some("abcdef".into()),
            igneous: Some("xyz".into()),
            nw: true,
        };
        let header = cookies.to_header();
        assert!(header.contains("ipb_member_id=12345"));
        assert!(header.contains("ipb_pass_hash=abcdef"));
        assert!(header.contains("igneous=xyz"));
        assert!(header.contains("nw=1"));
    }

    #[test]
    fn test_cookie_exhentai_capable() {
        let full = EhCookies {
            ipb_member_id: Some("1".into()),
            ipb_pass_hash: Some("h".into()),
            igneous: Some("i".into()),
            nw: true,
        };
        assert!(full.is_exhentai_capable());

        let partial = EhCookies {
            ipb_member_id: Some("1".into()),
            ipb_pass_hash: None,
            igneous: None,
            nw: true,
        };
        assert!(!partial.is_exhentai_capable());
    }

    #[test]
    fn test_category_bitmask() {
        assert_eq!(EhCategory::bitmask_from_str("doujinshi,manga"), 3);
        assert_eq!(EhCategory::bitmask_from_str("doujinshi"), 1);
        assert_eq!(EhCategory::bitmask_from_str("all"), 0); // unknown = 0
    }

    #[test]
    fn test_raw_meta_into_gallery() {
        let raw = RawGalleryMeta {
            gid: 123,
            token: "abc".into(),
            title: "Test".into(),
            title_jpn: Some("テスト".into()),
            category: "Manga".into(),
            thumb: "https://ehgt.org/t.jpg".into(),
            uploader: "user".into(),
            posted: "1376143500".into(),
            filecount: "20".into(),
            filesize: 51210504,
            expunged: false,
            rating: "4.64".into(),
            tags: vec!["parody:touhou".into()],
        };
        let g = raw.into_gallery();
        assert_eq!(g.gid, 123);
        assert_eq!(g.posted, 1376143500);
        assert_eq!(g.filecount, 20);
        assert!((g.rating - 4.64).abs() < 0.001);
    }
}
```

- [ ] **Step 4: Create `eh_client/src/lib.rs`**

```rust
pub mod client;
pub mod error;
pub mod models;
pub mod parser;
pub mod telegraph;

pub use client::EhClient;
pub use error::{Error, Result};
pub use models::{EhCategory, EhCookies, EhGallery, EhGalleryRef};
pub use telegraph::TelegraphClient;
```

- [ ] **Step 5: Add `eh_client` to root `Cargo.toml`**

Add to `[workspace]` members:
```toml
members = [".", "migration", "pixiv_client", "booru_client", "eh_client"]
```

Add to root `[dependencies]`:
```toml
eh_client = { path = "eh_client" }
```

- [ ] **Step 6: Create placeholder files so crate compiles**

Create `eh_client/src/parser.rs`:
```rust
// Placeholder — implemented in Task 2
```

Create `eh_client/src/client.rs`:
```rust
// Placeholder — implemented in Task 3
```

Create `eh_client/src/telegraph.rs`:
```rust
// Placeholder — implemented in Task 4
```

- [ ] **Step 7: Verify crate compiles**

Run: `cargo check -p eh_client`
Expected: PASS (with warnings about unused code, that's fine)

- [ ] **Step 8: Run unit tests**

Run: `cargo test -p eh_client`
Expected: 4 tests PASS (cookie_header, cookie_exhentai_capable, category_bitmask, raw_meta_into_gallery)

- [ ] **Step 9: Commit**

```bash
git add eh_client/ Cargo.toml
git commit -m "feat: add eh_client crate scaffolding with models and error types"
```

---

## Task 2: HTML Parser (`eh_client/src/parser.rs`)

**Files:**
- Modify: `eh_client/src/parser.rs`

- [ ] **Step 1: Write failing tests for search result parsing**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH_HTML_SAMPLE: &str = r#"
    <div class="gl1t">
      <a href="https://e-hentai.org/g/123456/abcdef0123/">
        <img src="https://ehgt.org/t/abc.jpg" />
      </a>
      <div class="gl3t">
        <div class="glink">Sample Gallery Title</div>
      </div>
    </div>
    <div class="gl1t">
      <a href="https://e-hentai.org/g/789012/987654abcd/">
        <img src="https://ehgt.org/t/def.jpg" />
      </a>
      <div class="gl3t">
        <div class="glink">Second Gallery</div>
      </div>
    </div>
    "#;

    #[test]
    fn test_parse_search_results() {
        let results = parse_search_results(SEARCH_HTML_SAMPLE, "https://e-hentai.org");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].gid, 123456);
        assert_eq!(results[0].token, "abcdef0123");
        assert_eq!(results[0].title, "Sample Gallery Title");
        assert_eq!(results[1].gid, 789012);
        assert_eq!(results[1].token, "987654abcd");
    }

    #[test]
    fn test_parse_search_results_empty() {
        let results = parse_search_results("<html><body>No results</body></html>", "https://e-hentai.org");
        assert!(results.is_empty());
    }

    #[test]
    fn test_parse_archiver_key() {
        let html = r#"
        <a href="https://e-hentai.org/archiver.php?gid=123456&token=abcdef0123&or=470592--63bbddc729b849100ec24ab920ffdb84b6542b23">
          Archive Download
        </a>
        "#;
        let key = parse_archiver_key(html).expect("should find archiver key");
        assert_eq!(key, "470592--63bbddc729b849100ec24ab920ffdb84b6542b23");
    }

    #[test]
    fn test_parse_archiver_key_not_found() {
        let html = "<html><body>No archiver link</body></html>";
        assert!(parse_archiver_key(html).is_none());
    }

    #[test]
    fn test_parse_archive_redirect() {
        let html = r#"
        <script type="text/javascript">
        function gotonext() {
            document.getElementById("continue").innerHTML = "Please wait...";
            document.location = "http://123.45.67.89/archive/123456/abcdef0123/abcdef0123/0?autostart=1";
        }
        </script>
        "#;
        let url = parse_archive_redirect(html).expect("should find redirect URL");
        assert_eq!(url, "http://123.45.67.89/archive/123456/abcdef0123/abcdef0123/0?start=1");
    }

    #[test]
    fn test_parse_archive_redirect_not_found() {
        let html = "<html><body>No redirect</body></html>";
        assert!(parse_archive_redirect(html).is_none());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p eh_client parser -- --nocapture`
Expected: FAIL (functions not defined)

- [ ] **Step 3: Implement the parser**

```rust
use crate::error::{Error, Result};
use crate::models::EhGalleryRef;
use regex::Regex;
use std::sync::OnceLock;

fn search_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"<a\s+href="(https?://(?:e-hentai|exhentai)\.org/g/(\d+)/([0-9a-f]+)/?)"[^>]*>[\s\S]*?<div\s+class="glink">(.*?)</div>"#)
            .expect("invalid search regex")
    })
}

fn archiver_key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"archiver\.php\?gid=\d+&token=[0-9a-f]+&or=([0-9]+--[0-9a-f]+)"#)
            .expect("invalid archiver_key regex")
    })
}

fn archive_redirect_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"document\.location\s*=\s*"(https?://[^"]+/archive/[^"]+)"#)
            .expect("invalid archive_redirect regex")
    })
}

/// Parse search results HTML, extracting gallery references.
/// `base_url` is used to construct full gallery URLs if the HTML uses relative paths.
pub fn parse_search_results(html: &str, base_url: &str) -> Vec<EhGalleryRef> {
    let re = search_re();
    re.captures_iter(html)
        .filter_map(|cap| {
            let url = cap.get(1)?.as_str().to_string();
            let gid: u64 = cap.get(2)?.as_str().parse().ok()?;
            let token = cap.get(3)?.as_str().to_string();
            let title = cap.get(4)?.as_str().trim().to_string();
            // posted_ts is not easily extractable from search HTML without date parsing;
            // the metadata API will provide it. Set to 0 as placeholder.
            Some(EhGalleryRef {
                gid,
                token,
                title,
                url,
                posted_ts: 0,
            })
        })
        .collect()
}

/// Extract the archiver_key from a gallery HTML page.
/// Returns None if no archiver link is found.
pub fn parse_archiver_key(html: &str) -> Option<String> {
    let re = archiver_key_re();
    re.captures(html)
        .and_then(|cap| cap.get(1).map(|m| m.as_str().to_string()))
}

/// Extract the archive download URL from the archiver.php HTML response.
/// Replaces `autostart=1` with `start=1` in the redirect URL.
/// Returns None if no redirect is found.
pub fn parse_archive_redirect(html: &str) -> Option<String> {
    let re = archive_redirect_re();
    re.captures(html)
        .and_then(|cap| cap.get(1).map(|m| m.as_str().to_string()))
        .map(|url| url.replace("autostart=1", "start=1"))
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p eh_client parser`
Expected: 6 tests PASS

- [ ] **Step 5: Commit**

```bash
git add eh_client/src/parser.rs
git commit -m "feat(eh_client): add HTML parser for search results, archiver key, archive redirect"
```

---

## Task 3: EhClient (`eh_client/src/client.rs`)

**Files:**
- Modify: `eh_client/src/client.rs`

- [ ] **Step 1: Write failing tests for URL building**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_search_url_basic() {
        let client = EhClientBuilder::new()
            .base_url("https://e-hentai.org")
            .build();
        let url = client.build_search_url("female:elf", 0, 0);
        assert_eq!(url, "https://e-hentai.org/?f_search=female%3Aelf&f_cats=0&page=0");
    }

    #[test]
    fn test_build_search_url_with_cats() {
        let client = EhClientBuilder::new()
            .base_url("https://e-hentai.org")
            .build();
        let url = client.build_search_url("artist:wlop", 3, 2);
        assert!(url.contains("f_cats=3"));
        assert!(url.contains("page=2"));
    }

    #[test]
    fn test_build_api_url() {
        let client = EhClientBuilder::new()
            .base_url("https://e-hentai.org")
            .api_url("https://api.e-hentai.org/api.php")
            .build();
        assert_eq!(client.api_url, "https://api.e-hentai.org/api.php");
    }

    #[test]
    fn test_build_archiver_url() {
        let client = EhClientBuilder::new()
            .base_url("https://e-hentai.org")
            .build();
        let url = client.build_archiver_url(123456, "abcdef0123", "780x");
        assert_eq!(url, "https://e-hentai.org/archiver.php?gid=123456&token=abcdef0123&or=780x");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p eh_client client`
Expected: FAIL (types not defined)

- [ ] **Step 3: Implement the EhClient struct and builder**

```rust
use crate::error::{Error, Result};
use crate::models::{EhCookies, EhGallery, EhGalleryRef, RawApiResponse};
use crate::parser;
use reqwest::header::{COOKIE, USER_AGENT};
use std::path::Path;

const USER_AGENT_STR: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

pub struct EhClient {
    http: reqwest::Client,
    base_url: String,
    pub(crate) api_url: String,
    cookies: EhCookies,
    image_resolution: String,
}

impl EhClient {
    pub fn new(
        base_url: &str,
        api_url: &str,
        cookies: EhCookies,
        image_resolution: &str,
    ) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .user_agent(USER_AGENT_STR)
            .timeout(std::time::Duration::from_secs(30));

        // For exhentai, bind to IPv4 to avoid CloudFlare blocks
        if base_url.contains("exhentai") {
            builder = builder.local_address(std::net::Ipv4Addr::UNSPECIFIED.into());
        }

        let http = builder.build()?;
        Ok(Self {
            http,
            base_url: base_url.to_string(),
            api_url: api_url.to_string(),
            cookies,
            image_resolution: image_resolution.to_string(),
        })
    }

    /// Build a search URL from query, category bitmask, and page number.
    pub fn build_search_url(&self, query: &str, cats: u32, page: u32) -> String {
        format!(
            "{}/?f_search={}&f_cats={}&page={}",
            self.base_url,
            urlencoding::encode(query),
            cats,
            page
        )
    }

    /// Build an archiver.php URL.
    pub fn build_archiver_url(&self, gid: u64, token: &str, or: &str) -> String {
        format!(
            "{}/archiver.php?gid={}&token={}&or={}",
            self.base_url, gid, token, or
        )
    }

    /// Search for galleries. Returns gallery references parsed from HTML.
    pub async fn search(&self, query: &str, cats: u32, page: u32) -> Result<Vec<EhGalleryRef>> {
        let url = self.build_search_url(query, cats, page);
        let resp = self.http.get(&url).header(COOKIE, self.cookies.to_header()).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("search returned {}", status),
                status: status.as_u16(),
            });
        }
        let html = resp.text().await?;
        Ok(parser::parse_search_results(&html, &self.base_url))
    }

    /// Fetch gallery metadata via the api.php JSON endpoint.
    /// Max 25 galleries per request.
    pub async fn get_metadata(&self, gidlist: &[(u64, &str)]) -> Result<Vec<EhGallery>> {
        if gidlist.is_empty() {
            return Ok(Vec::new());
        }
        if gidlist.len() > 25 {
            return Err(Error::Other("get_metadata: max 25 galleries per request".into()));
        }

        let gidlist_json: Vec<[serde_json::Value; 2]> = gidlist
            .iter()
            .map(|(gid, token)| {
                [serde_json::json!(gid), serde_json::json!(token)]
            })
            .collect();

        let body = serde_json::json!({
            "method": "gdata",
            "gidlist": gidlist_json,
            "namespace": 1
        });

        let resp = self.http
            .post(&self.api_url)
            .header(COOKIE, self.cookies.to_header())
            .header("Content-Type", "application/json")
            .json(&body)
            .send().await?;

        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("api.php returned {}", status),
                status: status.as_u16(),
            });
        }

        let raw: RawApiResponse = resp.json().await?;
        Ok(raw.gmetadata.into_iter().map(|m| m.into_gallery()).collect())
    }

    /// Get the archiver_key for a gallery by scraping its HTML page.
    pub async fn get_archiver_key(&self, gid: u64, token: &str) -> Result<String> {
        let url = format!("{}/g/{}/{}/", self.base_url, gid, token);
        let resp = self.http.get(&url).header(COOKIE, self.cookies.to_header()).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("gallery page returned {}", status),
                status: status.as_u16(),
            });
        }
        let html = resp.text().await?;
        parser::parse_archiver_key(&html)
            .ok_or_else(|| Error::Parse("archiver_key not found in gallery page".into()))
    }

    /// Download a gallery archive ZIP to the specified path.
    /// `archiver_key_or_resolution` can be an archiver_key (from gallery page) or
    /// a resolution key like "780x", "980x", "1280x", "original".
    pub async fn download_archive(
        &self,
        gid: u64,
        token: &str,
        archiver_key: &str,
        dest: &Path,
    ) -> Result<u64> {
        // Step 1: POST to archiver.php to initiate download
        let archiver_url = self.build_archiver_url(gid, token, archiver_key);
        let form_data = if archiver_key.contains("--") {
            // It's a real archiver_key → download original
            vec![("dlcheck", "Download Original Archive")]
        } else {
            // It's a resolution key → download resampled
            vec![("dlcheck", "Download Resample Archive")]
        };

        let resp = self.http
            .post(&archiver_url)
            .header(COOKIE, self.cookies.to_header())
            .form(&form_data)
            .send().await?;

        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("archiver.php returned {}", status),
                status: status.as_u16(),
            });
        }

        let html = resp.text().await?;

        // Step 2: Parse the JS redirect URL
        let download_url = parser::parse_archive_redirect(&html)
            .ok_or_else(|| Error::Parse("archive redirect URL not found".into()))?;

        // Step 3: Download the ZIP file
        let resp = self.http
            .get(&download_url)
            .header(COOKIE, self.cookies.to_header())
            .send().await?;

        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("archive download returned {}", status),
                status: status.as_u16(),
            });
        }

        // Step 4: Save to file
        let bytes = resp.bytes().await?;
        std::fs::write(dest, &bytes)?;
        Ok(bytes.len() as u64)
    }

    /// Get the configured image resolution key.
    pub fn image_resolution(&self) -> &str {
        &self.image_resolution
    }
}

/// Builder for EhClient (useful for testing).
pub struct EhClientBuilder {
    base_url: String,
    api_url: String,
    cookies: EhCookies,
    image_resolution: String,
}

impl Default for EhClientBuilder {
    fn default() -> Self {
        Self {
            base_url: "https://e-hentai.org".into(),
            api_url: "https://api.e-hentai.org/api.php".into(),
            cookies: EhCookies { nw: true, ..Default::default() },
            image_resolution: "780x".into(),
        }
    }
}

impl EhClientBuilder {
    pub fn new() -> Self { Self::default() }
    pub fn base_url(mut self, url: &str) -> Self { self.base_url = url.into(); self }
    pub fn api_url(mut self, url: &str) -> Self { self.api_url = url.into(); self }
    pub fn cookies(mut self, c: EhCookies) -> Self { self.cookies = c; self }
    pub fn image_resolution(mut self, r: &str) -> Self { self.image_resolution = r.into(); self }
    pub fn build(self) -> EhClient {
        EhClient::new(&self.base_url, &self.api_url, self.cookies, &self.image_resolution)
            .expect("failed to build EhClient")
    }
}
```

- [ ] **Step 4: Verify `multipart` feature is already in `eh_client/Cargo.toml`**

The `eh_client/Cargo.toml` created in Task 1 already includes `multipart` in the reqwest features:
```toml
reqwest = { version = "0.12.28", default-features = false, features = ["json", "rustls-tls", "multipart"] }
```

No additional changes needed.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p eh_client client`
Expected: 4 tests PASS (build_search_url_basic, build_search_url_with_cats, build_api_url, build_archiver_url)

- [ ] **Step 6: Run full crate tests**

Run: `cargo test -p eh_client`
Expected: all tests PASS

- [ ] **Step 7: Commit**

```bash
git add eh_client/
git commit -m "feat(eh_client): add EhClient with search, metadata API, and archive download"
```

---

## Task 4: Telegraph Client (`eh_client/src/telegraph.rs`)

**Files:**
- Modify: `eh_client/src/telegraph.rs`

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_image_node() {
        let node = Node::img("https://telegra.ph/file/abc.jpg");
        assert_eq!(node.tag, "img");
        assert_eq!(node.attrs.unwrap()["src"], "https://telegra.ph/file/abc.jpg");
    }

    #[test]
    fn test_build_link_node() {
        let node = Node::link("https://example.com", "Next Page");
        assert_eq!(node.tag, "a");
        assert_eq!(node.attrs.unwrap()["href"], "https://example.com");
        assert_eq!(node.children.unwrap()[0], serde_json::json!("Next Page"));
    }

    #[test]
    fn test_content_size_estimate() {
        let nodes = vec![
            Node::img("https://telegra.ph/file/abc.jpg"),
            Node::img("https://telegra.ph/file/def.jpg"),
        ];
        let size = estimate_content_size(&nodes);
        // Each img node is roughly {"tag":"img","attrs":{"src":"url"}} ≈ 60-80 bytes
        assert!(size > 0);
    }

    #[test]
    fn test_split_image_urls_for_pages() {
        let urls: Vec<String> = (0..50).map(|i| format!("https://telegra.ph/file/{}.jpg", i)).collect();
        let chunks = split_for_pages(&urls, 1024); // 1KB limit for testing
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| !c.is_empty()));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p eh_client telegraph`
Expected: FAIL (types not defined)

- [ ] **Step 3: Implement the Telegraph client**

```rust
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};

/// A Telegraph content node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attrs: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<serde_json::Value>>,
}

impl Node {
    pub fn img(src: &str) -> Self {
        let mut attrs = serde_json::Map::new();
        attrs.insert("src".into(), serde_json::json!(src));
        Self {
            tag: "img".into(),
            attrs: Some(attrs),
            children: None,
        }
    }

    pub fn link(href: &str, text: &str) -> Self {
        let mut attrs = serde_json::Map::new();
        attrs.insert("href".into(), serde_json::json!(href));
        Self {
            tag: "a".into(),
            attrs: Some(attrs),
            children: Some(vec![serde_json::json!(text)]),
        }
    }

    pub fn paragraph(text: &str) -> Self {
        Self {
            tag: "p".into(),
            attrs: None,
            children: Some(vec![serde_json::json!(text)]),
        }
    }
}

/// Estimate serialized content size in bytes.
pub fn estimate_content_size(nodes: &[Node]) -> usize {
    serde_json::to_vec(nodes).map(|v| v.len()).unwrap_or(0)
}

/// Maximum content size per Telegraph page (64KB minus overhead).
const MAX_PAGE_CONTENT_BYTES: usize = 60_000;

/// Split image URLs into chunks that fit within the content size limit.
pub fn split_for_pages(urls: &[String], max_bytes: usize) -> Vec<Vec<String>> {
    if urls.is_empty() {
        return vec![];
    }
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut current_size = 0;
    for url in urls {
        let node = Node::img(url);
        let node_size = serde_json::to_vec(&node).map(|v| v.len()).unwrap_or(100);
        if current_size + node_size > max_bytes && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_size = 0;
        }
        current_size += node_size;
        current.push(url.clone());
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[derive(Debug, Deserialize)]
struct TelegraphResponse<T> {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    result: Option<T>,
}

#[derive(Debug, Deserialize)]
struct PageResult {
    url: String,
}

#[derive(Debug, Deserialize)]
struct UploadResult {
    src: Option<String>,
}

pub struct TelegraphClient {
    http: reqwest::Client,
    access_token: String,
}

impl TelegraphClient {
    pub fn new(access_token: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("failed to build telegraph http client"),
            access_token,
        }
    }

    /// Upload an image to Telegraph. Returns the full URL.
    pub async fn upload_image(&self, image_data: &[u8], filename: &str) -> Result<String> {
        let part = reqwest::multipart::Part::bytes(image_data.to_vec())
            .file_name(filename.to_string())
            .mime_str("image/jpeg")
            .map_err(|e| Error::Other(format!("mime error: {e}")))?;
        let form = reqwest::multipart::Form::new().part("file", part);

        let resp = self.http
            .post("https://telegra.ph/upload")
            .multipart(form)
            .send().await?;

        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("telegraph upload returned {}", status),
                status: status.as_u16(),
            });
        }

        let results: Vec<UploadResult> = resp.json().await?;
        if let Some(first) = results.first() {
            if let Some(ref src) = first.src {
                return Ok(format!("https://telegra.ph{}", src));
            }
        }
        Err(Error::Parse("telegraph upload returned no src".into()))
    }

    /// Create a Telegraph page. Returns the page URL.
    pub async fn create_page(&self, title: &str, content: &[Node]) -> Result<String> {
        let content_json = serde_json::to_value(content)?;
        let form = vec![
            ("access_token", self.access_token.as_str()),
            ("title", title),
            ("content", &content_json.to_string()),
            ("return_content", "false"),
        ];

        let resp = self.http
            .post("https://api.telegra.ph/createPage")
            .form(&form)
            .send().await?;

        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("createPage returned {}", status),
                status: status.as_u16(),
            });
        }

        let telegraph_resp: TelegraphResponse<PageResult> = resp.json().await?;
        if telegraph_resp.ok {
            if let Some(result) = telegraph_resp.result {
                return Ok(result.url);
            }
        }
        Err(Error::Api {
            message: telegraph_resp.error.unwrap_or_else(|| "unknown error".into()),
            status: 0,
        })
    }

    /// Create a gallery page from image URLs. Splits into multiple pages if needed.
    /// Returns the first page URL (with "Next Page" links to subsequent pages).
    pub async fn create_gallery_page(&self, title: &str, image_urls: &[String]) -> Result<String> {
        if image_urls.is_empty() {
            return Err(Error::Other("no images to upload".into()));
        }

        let chunks = split_for_pages(image_urls, MAX_PAGE_CONTENT_BYTES);
        if chunks.len() == 1 {
            let nodes: Vec<Node> = chunks[0].iter().map(|url| Node::img(url)).collect();
            return self.create_page(title, &nodes).await;
        }

        // Multi-page: create in reverse order, linking to the next page
        let mut next_url: Option<String> = None;
        for chunk in chunks.iter().rev() {
            let mut nodes: Vec<Node> = Vec::new();
            if let Some(ref next) = next_url {
                nodes.push(Node::link(next, "Next Page →"));
            }
            for url in chunk {
                nodes.push(Node::img(url));
            }
            let page_title = if next_url.is_some() {
                format!("{} (continued)", title)
            } else {
                title.to_string()
            };
            let url = self.create_page(&page_title, &nodes).await?;
            next_url = Some(url);
        }

        // Return the last-created URL (which is the first page due to reverse order)
        Ok(next_url.unwrap_or_else(|| image_urls[0].clone()))
    }
}
```

- [ ] **Step 4: Verify `multipart` feature is included**

The `eh_client/Cargo.toml` already includes `multipart` in the reqwest features from Task 1. No changes needed.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p eh_client telegraph`
Expected: 4 tests PASS

- [ ] **Step 6: Run full crate tests**

Run: `cargo test -p eh_client`
Expected: all tests PASS

- [ ] **Step 7: Commit**

```bash
git add eh_client/
git commit -m "feat(eh_client): add Telegraph client with image upload and gallery page creation"
```

---

## Task 5: DB Types — EhFilter, EhTaskKey, TaskType, SubscriptionState

**Files:**
- Create: `src/db/types/eh_filter.rs`
- Create: `src/db/types/eh_task_key.rs`
- Modify: `src/db/types/task_type.rs`
- Modify: `src/db/types/state.rs`
- Modify: `src/db/types/mod.rs`

- [ ] **Step 1: Write failing tests for EhFilter**

Create `src/db/types/eh_filter.rs` with tests:

```rust
use chrono::Utc;
use eh_client::EhGallery;
use serde::{Deserialize, Serialize};
use sea_orm::entity::prelude::*;

/// Filter for e-hentai subscriptions.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EhFilter {
    pub min_rating: Option<u8>,
    pub min_pages: Option<u32>,
    pub max_pages: Option<u32>,
    pub telegraph: bool,
}

impl EhFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.min_rating.is_none()
            && self.min_pages.is_none()
            && self.max_pages.is_none()
            && !self.telegraph
    }

    /// Encode a signature string for task value deduplication.
    /// Fixed order: r{min_rating} p{min_pages} P{max_pages}
    pub fn task_value_signature(&self) -> String {
        let mut sig = String::new();
        if let Some(r) = self.min_rating {
            sig.push_str(&format!("r{}", r));
        }
        if let Some(p) = self.min_pages {
            sig.push_str(&format!("p{}", p));
        }
        if let Some(p) = self.max_pages {
            sig.push_str(&format!("P{}", p));
        }
        sig
    }

    /// True if this filter has a rating filter (triggers 48h scan mode).
    pub fn has_rating_filter(&self) -> bool {
        self.min_rating.is_some()
    }

    /// Check if a gallery matches this filter.
    pub fn matches(&self, gallery: &EhGallery) -> bool {
        if let Some(min_rating) = self.min_rating {
            if (gallery.rating as u8) < min_rating {
                return false;
            }
        }
        if let Some(min_pages) = self.min_pages {
            if gallery.filecount < min_pages {
                return false;
            }
        }
        if let Some(max_pages) = self.max_pages {
            if gallery.filecount > max_pages {
                return false;
            }
        }
        true
    }

    /// Aggregate multiple filters into the loosest one.
    /// Takes min of min_rating, min of min_pages, max of max_pages.
    pub fn aggregate(filters: &[&EhFilter]) -> Self {
        if filters.is_empty() {
            return Self::default();
        }
        let mut result = Self::default();
        result.telegraph = filters.iter().any(|f| f.telegraph);
        for f in filters {
            if let Some(r) = f.min_rating {
                result.min_rating = Some(result.min_rating.map_or(r, |existing| existing.min(r)));
            }
            if let Some(p) = f.min_pages {
                result.min_pages = Some(result.min_pages.map_or(p, |existing| existing.min(p)));
            }
            if let Some(p) = f.max_pages {
                result.max_pages = Some(result.max_pages.map_or(p, |existing| existing.max(p)));
            }
        }
        result
    }

    /// Format for display in user-facing messages.
    pub fn format_for_display(&self) -> String {
        let mut parts = Vec::new();
        if let Some(r) = self.min_rating {
            parts.push(format!("rating>={}", r));
        }
        if let Some(p) = self.min_pages {
            parts.push(format!("pages>={}", p));
        }
        if let Some(p) = self.max_pages {
            parts.push(format!("pages<={}", p));
        }
        if self.telegraph {
            parts.push("telegraph=on".into());
        }
        if parts.is_empty() {
            "no filters".to_string()
        } else {
            parts.join(", ")
        }
    }
}

impl FromJsonQueryResult for EhFilter {}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_gallery(rating: f64, filecount: u32) -> EhGallery {
        EhGallery {
            gid: 1,
            token: "abc".into(),
            title: "Test".into(),
            title_jpn: None,
            category: "Manga".into(),
            thumb: "".into(),
            uploader: "user".into(),
            posted: 0,
            filecount,
            filesize: 0,
            expunged: false,
            rating,
            tags: vec![],
        }
    }

    #[test]
    fn test_eh_filter_empty() {
        assert!(EhFilter::new().is_empty());
        assert!(!EhFilter { min_rating: Some(4), ..Default::default() }.is_empty());
    }

    #[test]
    fn test_task_value_signature() {
        let f = EhFilter { min_rating: Some(4), min_pages: Some(20), max_pages: None, telegraph: false };
        assert_eq!(f.task_value_signature(), "r4p20");

        let f2 = EhFilter { min_rating: None, min_pages: None, max_pages: Some(500), telegraph: true };
        assert_eq!(f2.task_value_signature(), "P500");
    }

    #[test]
    fn test_has_rating_filter() {
        assert!(EhFilter { min_rating: Some(4), ..Default::default() }.has_rating_filter());
        assert!(!EhFilter { min_pages: Some(20), ..Default::default() }.has_rating_filter());
    }

    #[test]
    fn test_matches_rating() {
        let f = EhFilter { min_rating: Some(4), ..Default::default() };
        assert!(f.matches(&make_gallery(4.5, 10)));
        assert!(f.matches(&make_gallery(4.0, 10)));
        assert!(!f.matches(&make_gallery(3.5, 10)));
    }

    #[test]
    fn test_matches_pages() {
        let f = EhFilter { min_pages: Some(20), max_pages: Some(100), ..Default::default() };
        assert!(f.matches(&make_gallery(4.0, 50)));
        assert!(!f.matches(&make_gallery(4.0, 10)));
        assert!(!f.matches(&make_gallery(4.0, 150)));
    }

    #[test]
    fn test_aggregate() {
        let f1 = EhFilter { min_rating: Some(4), min_pages: Some(20), max_pages: None, telegraph: false };
        let f2 = EhFilter { min_rating: Some(3), min_pages: Some(10), max_pages: Some(100), telegraph: true };
        let agg = EhFilter::aggregate(&[&f1, &f2]);
        assert_eq!(agg.min_rating, Some(3));
        assert_eq!(agg.min_pages, Some(10));
        assert_eq!(agg.max_pages, Some(100));
        assert!(agg.telegraph);
    }

    #[test]
    fn test_format_for_display() {
        let f = EhFilter { min_rating: Some(4), telegraph: true, ..Default::default() };
        let s = f.format_for_display();
        assert!(s.contains("rating>=4"));
        assert!(s.contains("telegraph=on"));
    }
}
```

- [ ] **Step 2: Write failing tests for EhTaskKey**

Create `src/db/types/eh_task_key.rs`:

```rust
use super::eh_filter::EhFilter;

/// Task key for e-hentai subscriptions.
/// Encodes query, category bitmask, and filter signature into the task value string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EhTaskKey {
    pub query: String,
    pub category_bitmask: u32,
    pub filter_sig: String,
}

impl EhTaskKey {
    pub fn new(query: &str, category_bitmask: u32, eh_filter: &EhFilter) -> Self {
        Self {
            query: query.to_string(),
            category_bitmask,
            filter_sig: eh_filter.task_value_signature(),
        }
    }

    /// Encode to task value string: `eh:<query>|c=<bitmask>|f=<filter_sig>`
    pub fn to_task_value(&self) -> String {
        let mut val = format!("eh:{}", self.query);
        if self.category_bitmask > 0 {
            val.push_str(&format!("|c={}", self.category_bitmask));
        }
        if !self.filter_sig.is_empty() {
            val.push_str(&format!("|f={}", self.filter_sig));
        }
        val
    }

    /// Parse a task value string back into an EhTaskKey.
    pub fn parse(value: &str) -> Option<Self> {
        let (head, rest) = value.split_once('|').unwrap_or((value, ""));
        let (prefix, query) = head.split_once(':')?;
        if prefix != "eh" {
            return None;
        }

        let mut category_bitmask = 0u32;
        let mut filter_sig = String::new();
        let mut seen_c = false;
        let mut seen_f = false;

        for segment in rest.split('|') {
            if segment.is_empty() {
                continue;
            }
            if let Some(val) = segment.strip_prefix("c=") {
                if seen_c {
                    return None;
                }
                seen_c = true;
                category_bitmask = val.parse().ok()?;
            } else if let Some(val) = segment.strip_prefix("f=") {
                if seen_f {
                    return None;
                }
                seen_f = true;
                filter_sig = val.to_string();
            } else {
                return None;
            }
        }

        Some(Self {
            query: query.to_string(),
            category_bitmask,
            filter_sig,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_filter() {
        let key = EhTaskKey::new("female:elf", 0, &EhFilter::default());
        assert_eq!(key.to_task_value(), "eh:female:elf");
    }

    #[test]
    fn test_with_cats() {
        let key = EhTaskKey::new("artist:wlop", 3, &EhFilter::default());
        assert_eq!(key.to_task_value(), "eh:artist:wlop|c=3");
    }

    #[test]
    fn test_with_filter() {
        let filter = EhFilter { min_rating: Some(4), ..Default::default() };
        let key = EhTaskKey::new("female:elf", 0, &filter);
        assert_eq!(key.to_task_value(), "eh:female:elf|f=r4");
    }

    #[test]
    fn test_with_cats_and_filter() {
        let filter = EhFilter { min_rating: Some(4), min_pages: Some(20), ..Default::default() };
        let key = EhTaskKey::new("parody:touhou", 3, &filter);
        assert_eq!(key.to_task_value(), "eh:parody:touhou|c=3|f=r4p20");
    }

    #[test]
    fn test_roundtrip() {
        let key = EhTaskKey::new("female:elf", 3, &EhFilter { min_rating: Some(4), ..Default::default() });
        let val = key.to_task_value();
        let parsed = EhTaskKey::parse(&val).expect("should parse");
        assert_eq!(key, parsed);
    }

    #[test]
    fn test_parse_invalid_prefix() {
        assert!(EhTaskKey::parse("booru:tags").is_none());
    }

    #[test]
    fn test_parse_duplicate_c() {
        assert!(EhTaskKey::parse("eh:query|c=1|c=2").is_none());
    }

    #[test]
    fn test_parse_unknown_segment() {
        assert!(EhTaskKey::parse("eh:query|x=1").is_none());
    }
}
```

- [ ] **Step 3: Add `Ehentai` variant to TaskType**

Modify `src/db/types/task_type.rs`, add after the `BooruRanking("booru_ranking")` variant:

```rust
    Ehentai("ehentai"),
```

And add to the Display impl's match (if there's a match arm list).

- [ ] **Step 4: Add `EhTag` variant to SubscriptionState**

Modify `src/db/types/state.rs`. Add the EhTagState struct and variant:

```rust
/// State for e-hentai tag subscriptions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EhTagState {
    pub pushed_gids: Vec<u64>,
    pub latest_posted_ts: i64,
    #[serde(default)]
    pub retry_count: u8,
}

impl EhTagState {
    pub fn cleared(latest_posted_ts: i64) -> Self {
        Self {
            pushed_gids: Vec::new(),
            latest_posted_ts,
            retry_count: 0,
        }
    }

    pub fn with_retry_increment(&self) -> Self {
        Self {
            retry_count: self.retry_count.saturating_add(1),
            ..self.clone()
        }
    }

    pub fn should_abandon_queue(&self, max_retry_count: u8) -> bool {
        max_retry_count == 0 || self.retry_count >= max_retry_count
    }

    pub fn add_pushed_gid(&mut self, gid: u64) {
        if !self.pushed_gids.contains(&gid) {
            self.pushed_gids.push(gid);
        }
    }

    pub fn trim_pushed(&mut self, cap: usize) {
        if self.pushed_gids.len() > cap {
            let drain = self.pushed_gids.len() - cap;
            self.pushed_gids.drain(..drain);
        }
    }
}

/// Queued e-hentai gallery for download.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuedEhGallery {
    pub gid: u64,
    pub token: String,
    pub title: String,
    pub title_jpn: Option<String>,
    pub category: String,
    pub thumb: String,
    pub uploader: String,
    pub posted: i64,
    pub filecount: u32,
    pub filesize: u64,
    pub rating: f64,
    pub tags: Vec<String>,
}
```

Add to the `SubscriptionState` enum:

```rust
    EhTag(EhTagState),
```

Add `use serde::{Deserialize, Serialize};` if not already imported in the module.

Add tests:

```rust
#[cfg(test)]
mod eh_tag_state_tests {
    use super::*;

    #[test]
    fn test_cleared() {
        let s = EhTagState::cleared(1234567890);
        assert!(s.pushed_gids.is_empty());
        assert_eq!(s.latest_posted_ts, 1234567890);
        assert_eq!(s.retry_count, 0);
    }

    #[test]
    fn test_with_retry_increment() {
        let s = EhTagState { pushed_gids: vec![1], latest_posted_ts: 100, retry_count: 2 };
        let s2 = s.with_retry_increment();
        assert_eq!(s2.retry_count, 3);
        assert_eq!(s2.pushed_gids, vec![1]);
    }

    #[test]
    fn test_retry_saturate() {
        let s = EhTagState { pushed_gids: vec![], latest_posted_ts: 0, retry_count: 255 };
        let s2 = s.with_retry_increment();
        assert_eq!(s2.retry_count, 255);
    }

    #[test]
    fn test_should_abandon_queue() {
        let s = EhTagState { pushed_gids: vec![], latest_posted_ts: 0, retry_count: 3 };
        assert!(s.should_abandon_queue(3));
        assert!(!s.should_abandon_queue(0));
    }

    #[test]
    fn test_add_pushed_gid_dedup() {
        let mut s = EhTagState::cleared(0);
        s.add_pushed_gid(100);
        s.add_pushed_gid(100);
        s.add_pushed_gid(200);
        assert_eq!(s.pushed_gids, vec![100, 200]);
    }

    #[test]
    fn test_trim_pushed() {
        let mut s = EhTagState { pushed_gids: vec![1, 2, 3, 4, 5], latest_posted_ts: 0, retry_count: 0 };
        s.trim_pushed(3);
        assert_eq!(s.pushed_gids, vec![3, 4, 5]);
    }
}
```

- [ ] **Step 5: Register new modules in `src/db/types/mod.rs`**

Add:
```rust
pub mod eh_filter;
pub mod eh_task_key;

pub use eh_filter::EhFilter;
pub use eh_task_key::EhTaskKey;
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p pixivbot -- eh_filter eh_task_key eh_tag_state`
Expected: all tests PASS

- [ ] **Step 7: Run `cargo check` to ensure everything compiles**

Run: `cargo check`
Expected: PASS

- [ ] **Step 8: Commit**

```bash
git add src/db/types/ Cargo.toml
git commit -m "feat: add EhFilter, EhTaskKey, EhTagState types and TaskType::Ehentai"
```

---

## Task 6: Migration + EhDownloadQueue Entity + Repo

**Files:**
- Create: `migration/src/m20260626_000000_add_ehentai.rs`
- Modify: `migration/src/lib.rs`
- Create: `src/db/entities/eh_download_queue.rs`
- Modify: `src/db/entities/mod.rs`
- Create: `src/db/repo/eh_download_queue.rs`
- Modify: `src/db/repo/mod.rs`
- Modify: `src/db/repo/subscriptions.rs`
- Modify: `src/db/entities/subscriptions.rs`

- [ ] **Step 1: Create migration file**

Create `migration/src/m20260626_000000_add_ehentai.rs`:

```rust
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // 1. Add eh_filter column to subscriptions
        manager
            .alter_table(
                Table::alter()
                    .table(Subscriptions::Table)
                    .add_column(ColumnDef::new(Subscriptions::EhFilter).json().null())
                    .to_owned(),
            )
            .await?;

        // 2. Create eh_download_queue table
        manager
            .create_table(
                Table::create()
                    .table(EhDownloadQueue::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(EhDownloadQueue::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(EhDownloadQueue::ChatId).integer().not_null())
                    .col(ColumnDef::new(EhDownloadQueue::Gid).big_integer().not_null())
                    .col(ColumnDef::new(EhDownloadQueue::Token).text().not_null())
                    .col(ColumnDef::new(EhDownloadQueue::Title).text().not_null())
                    .col(
                        ColumnDef::new(EhDownloadQueue::Telegraph)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .col(ColumnDef::new(EhDownloadQueue::Source).text().not_null())
                    .col(
                        ColumnDef::new(EhDownloadQueue::Status)
                            .text()
                            .not_null()
                            .default("pending"),
                    )
                    .col(ColumnDef::new(EhDownloadQueue::FileSize).big_integer().null())
                    .col(ColumnDef::new(EhDownloadQueue::Error).text().null())
                    .col(
                        ColumnDef::new(EhDownloadQueue::RetryCount)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(EhDownloadQueue::CreatedAt)
                            .timestamp()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .col(ColumnDef::new(EhDownloadQueue::StartedAt).timestamp().null())
                    .col(ColumnDef::new(EhDownloadQueue::CompletedAt).timestamp().null())
                    .to_owned(),
            )
            .await?;

        // 3. Create indices
        manager
            .create_index(
                Index::create()
                    .name("idx_eh_dlq_status")
                    .table(EhDownloadQueue::Table)
                    .col(EhDownloadQueue::Status)
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_eh_dlq_completed")
                    .table(EhDownloadQueue::Table)
                    .col(EhDownloadQueue::CompletedAt)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(EhDownloadQueue::Table).to_owned())
            .await?;

        manager
            .alter_table(
                Table::alter()
                    .table(Subscriptions::Table)
                    .drop_column(Subscriptions::EhFilter)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }
}

#[derive(DeriveIden)]
enum Subscriptions {
    Table,
    EhFilter,
}

#[derive(DeriveIden)]
enum EhDownloadQueue {
    Table,
    Id,
    ChatId,
    Gid,
    Token,
    Title,
    Telegraph,
    Source,
    Status,
    FileSize,
    Error,
    RetryCount,
    CreatedAt,
    StartedAt,
    CompletedAt,
}
```

- [ ] **Step 2: Register migration in `migration/src/lib.rs`**

Add `mod m20260626_000000_add_ehentai;` to module declarations.

Add `Box::new(m20260626_000000_add_ehentai::Migration),` to the `migrations()` vector (after the last existing migration).

- [ ] **Step 3: Create `src/db/entities/eh_download_queue.rs`**

```rust
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "eh_download_queue")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub chat_id: i64,
    pub gid: i64,
    pub token: String,
    pub title: String,
    pub telegraph: bool,
    pub source: String,
    pub status: String,
    pub file_size: Option<i64>,
    pub error: Option<String>,
    pub retry_count: i32,
    pub created_at: DateTime,
    pub started_at: Option<DateTime>,
    pub completed_at: Option<DateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// Download status constants.
pub const STATUS_PENDING: &str = "pending";
pub const STATUS_DOWNLOADING: &str = "downloading";
pub const STATUS_DONE: &str = "done";
pub const STATUS_FAILED: &str = "failed";

/// Source constants.
pub const SOURCE_SUBSCRIPTION: &str = "subscription";
pub const SOURCE_DIRECT: &str = "direct";
```

- [ ] **Step 4: Register entity in `src/db/entities/mod.rs`**

Add:
```rust
pub mod eh_download_queue;
```

- [ ] **Step 5: Add `eh_filter` column to subscriptions entity**

Modify `src/db/entities/subscriptions.rs`. Add to the `Model` struct:

```rust
    #[serde(default)]
    pub eh_filter: Option<EhFilter>,
```

Add the import if needed.

- [ ] **Step 6: Create `src/db/repo/eh_download_queue.rs`**

```rust
use crate::db::entities::eh_download_queue;
use crate::db::repo::Repo;
use anyhow::Result;
use chrono::{DateTime, Utc};
use sea_orm::*;

impl Repo {
    pub async fn enqueue_download(
        &self,
        chat_id: i64,
        gid: u64,
        token: &str,
        title: &str,
        telegraph: bool,
        source: &str,
    ) -> Result<()> {
        let model = eh_download_queue::ActiveModel {
            chat_id: Set(chat_id),
            gid: Set(gid as i64),
            token: Set(token.to_string()),
            title: Set(title.to_string()),
            telegraph: Set(telegraph),
            source: Set(source.to_string()),
            status: Set(eh_download_queue::STATUS_PENDING.to_string()),
            file_size: Set(None),
            error: Set(None),
            retry_count: Set(0),
            created_at: Set(chrono::Utc::now()),
            started_at: Set(None),
            completed_at: Set(None),
            ..Default::default()
        };
        eh_download_queue::Entity::insert(model).exec(&self.db).await?;
        Ok(())
    }

    pub async fn get_next_pending_download(
        &self,
    ) -> Result<Option<eh_download_queue::Model>> {
        let result = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(eh_download_queue::STATUS_PENDING))
            .order_by_asc(eh_download_queue::Column::CreatedAt)
            .one(&self.db)
            .await?;
        Ok(result)
    }

    pub async fn mark_download_started(&self, id: i32) -> Result<()> {
        let now = chrono::Utc::now();
        eh_download_queue::Entity::update_many()
            .col_expr(eh_download_queue::Column::Status, Expr::value(eh_download_queue::STATUS_DOWNLOADING))
            .col_expr(eh_download_queue::Column::StartedAt, Expr::value(now))
            .filter(eh_download_queue::Column::Id.eq(id))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    pub async fn mark_download_done(&self, id: i32, file_size: u64) -> Result<()> {
        let now = chrono::Utc::now();
        eh_download_queue::Entity::update_many()
            .col_expr(eh_download_queue::Column::Status, Expr::value(eh_download_queue::STATUS_DONE))
            .col_expr(eh_download_queue::Column::FileSize, Expr::value(file_size as i64))
            .col_expr(eh_download_queue::Column::CompletedAt, Expr::value(now))
            .filter(eh_download_queue::Column::Id.eq(id))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    pub async fn mark_download_failed(&self, id: i32, error: &str) -> Result<()> {
        eh_download_queue::Entity::update_many()
            .col_expr(eh_download_queue::Column::Status, Expr::value(eh_download_queue::STATUS_PENDING))
            .col_expr(eh_download_queue::Column::Error, Expr::value(error))
            .col_expr(
                eh_download_queue::Column::RetryCount,
                Expr::col(eh_download_queue::Column::RetryCount).add(1),
            )
            .filter(eh_download_queue::Column::Id.eq(id))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    pub async fn mark_download_permanently_failed(&self, id: i32, error: &str) -> Result<()> {
        eh_download_queue::Entity::update_many()
            .col_expr(eh_download_queue::Column::Status, Expr::value(eh_download_queue::STATUS_FAILED))
            .col_expr(eh_download_queue::Column::Error, Expr::value(error))
            .filter(eh_download_queue::Column::Id.eq(id))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    pub async fn get_downloaded_bytes_in_window(
        &self,
        window_start: DateTime<Utc>,
    ) -> Result<u64> {
        let result: Option<i64> = eh_download_queue::Entity::find()
            .select_only()
            .column_sum(eh_download_queue::Column::FileSize)
            .filter(eh_download_queue::Column::Status.eq(eh_download_queue::STATUS_DONE))
            .filter(eh_download_queue::Column::CompletedAt.gte(window_start))
            .into_tuple()
            .one(&self.db)
            .await?;

        Ok(result.unwrap_or(0) as u64)
    }

    pub async fn count_pending_downloads(&self) -> Result<u64> {
        let count = eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::Status.eq(eh_download_queue::STATUS_PENDING))
            .count(&self.db)
            .await?;
        Ok(count)
    }

    pub async fn reset_stale_downloading(&self) -> Result<u64> {
        let result = eh_download_queue::Entity::update_many()
            .col_expr(eh_download_queue::Column::Status, Expr::value(eh_download_queue::STATUS_PENDING))
            .col_expr(eh_download_queue::Column::StartedAt, Expr::value(Option::<DateTime<Utc>>::None))
            .filter(eh_download_queue::Column::Status.eq(eh_download_queue::STATUS_DOWNLOADING))
            .exec(&self.db)
            .await?;
        Ok(result.rows_affected)
    }
}
```

- [ ] **Step 7: Register repo module in `src/db/repo/mod.rs`**

Add:
```rust
pub mod eh_download_queue;
```

- [ ] **Step 8: Add `upsert_eh_subscription` to `src/db/repo/subscriptions.rs`**

```rust
pub async fn upsert_eh_subscription(
    &self,
    chat_id: i64,
    task_id: i32,
    filter_tags: TagFilter,
    eh_filter: Option<EhFilter>,
) -> Result<subscriptions::Model> {
    let now = chrono::Utc::now();
    let model = subscriptions::ActiveModel {
        chat_id: Set(chat_id),
        task_id: Set(task_id),
        filter_tags: Set(filter_tags),
        eh_filter: Set(eh_filter),
        booru_filter: Set(None),
        latest_data: Set(None),
        created_at: Set(now),
        ..Default::default()
    };

    subscriptions::Entity::insert(model)
        .on_conflict(
            sea_orm::sea_query::OnConflict::columns([
                subscriptions::Column::ChatId,
                subscriptions::Column::TaskId,
            ])
            .update_columns([
                subscriptions::Column::FilterTags,
                subscriptions::Column::EhFilter,
            ])
            .to_owned(),
        )
        .exec(&self.db)
        .await?;

    let result = subscriptions::Entity::find()
        .filter(
            Condition::all()
                .add(subscriptions::Column::ChatId.eq(chat_id))
                .add(subscriptions::Column::TaskId.eq(task_id)),
        )
        .one(&self.db)
        .await?
        .ok_or_else(|| anyhow::anyhow!("failed to find upserted subscription"))?;

    Ok(result)
}
```

- [ ] **Step 9: Run `cargo check`**

Run: `cargo check`
Expected: PASS

- [ ] **Step 10: Run migration tests**

Run: `cargo test -p migration`
Expected: PASS (if migration tests exist; otherwise just check compilation)

- [ ] **Step 11: Commit**

```bash
git add migration/ src/db/
git commit -m "feat: add ehentai migration, eh_download_queue entity and repo, upsert_eh_subscription"
```

---

## Task 7: Config + Notifier Extensions + Caption Helper

**Files:**
- Modify: `src/config.rs`
- Modify: `config.toml.example`
- Modify: `src/bot/notifier.rs`
- Create: `src/bot/notifier/document.rs`
- Modify: `src/utils/caption.rs`

- [ ] **Step 1: Add `EhentaiConfig` to `src/config.rs`**

Add the struct and Default impl (from spec). Add `pub ehentai: EhentaiConfig` to the `Config` struct with `#[serde(default)]`.

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct EhentaiConfig {
    pub site: String,
    pub ipb_member_id: Option<String>,
    pub ipb_pass_hash: Option<String>,
    pub igneous: Option<String>,
    pub image_resolution: String,
    pub min_interval_sec: u64,
    pub max_interval_sec: u64,
    pub telegraph_access_token: Option<String>,
    pub max_push_per_tick: usize,
    pub max_retry_count: u8,
    pub scan_window_hours: u64,
    pub download_rate_limit_gb: u64,
    pub download_rate_window_hours: u64,
    pub download_poll_interval_sec: u64,
}

impl Default for EhentaiConfig {
    fn default() -> Self {
        Self {
            site: String::new(), // empty = disabled
            ipb_member_id: None,
            ipb_pass_hash: None,
            igneous: None,
            image_resolution: "780x".into(),
            min_interval_sec: 1800,
            max_interval_sec: 3600,
            telegraph_access_token: None,
            max_push_per_tick: 3,
            max_retry_count: 3,
            scan_window_hours: 48,
            download_rate_limit_gb: 10,
            download_rate_window_hours: 24,
            download_poll_interval_sec: 60,
        }
    }
}

impl EhentaiConfig {
    /// Returns true if the feature is enabled (site is configured).
    pub fn is_enabled(&self) -> bool {
        !self.site.is_empty()
    }

    /// Returns true if exhentai is configured with all required cookies.
    pub fn is_exhentai_ready(&self) -> bool {
        self.site == "exhentai"
            && self.ipb_member_id.is_some()
            && self.ipb_pass_hash.is_some()
            && self.igneous.is_some()
    }

    /// Returns the base URL based on site config.
    pub fn base_url(&self) -> &str {
        if self.site == "exhentai" {
            "https://exhentai.org"
        } else {
            "https://e-hentai.org"
        }
    }

    pub fn api_url(&self) -> &str {
        "https://api.e-hentai.org/api.php"
    }
}
```

Add to `Config` struct:
```rust
    #[serde(default)]
    pub ehentai: EhentaiConfig,
```

- [ ] **Step 2: Add `[ehentai]` section to `config.toml.example`**

Append after the `[[booru.sites]]` section:

```toml

[ehentai]
# Omit this section to disable e-hentai/exhentai features
# site = "e-hentai"                     # "e-hentai" or "exhentai"
# ipb_member_id = ""                    # required for exhentai
# ipb_pass_hash = ""                    # required for exhentai
# igneous = ""                          # required for exhentai
# image_resolution = "780x"            # 780x, 980x, 1280x, 1600x, 2400x, original
# min_interval_sec = 1800              # 30 min
# max_interval_sec = 3600              # 1 hour
# telegraph_access_token = ""          # optional, for Telegraph uploads
# max_push_per_tick = 3                # max galleries to enqueue per tick
# max_retry_count = 3                  # download retry limit
# scan_window_hours = 48               # 48h scan window for rating filters
# download_rate_limit_gb = 10          # max GB downloaded per 24h window
# download_rate_window_hours = 24      # rate-limit window duration
# download_poll_interval_sec = 60      # how often the download processor drains the queue
```

- [ ] **Step 3: Create `src/bot/notifier/document.rs`**

```rust
use crate::bot::notifier::Notifier;
use anyhow::Result;
use std::path::Path;
use teloxide::prelude::*;
use teloxide::types::{ChatAction, ChatId, ParseMode};

impl Notifier {
    /// Send a document (file) to a chat. Returns message_id on success.
    pub async fn notify_with_document(
        &self,
        chat_id: ChatId,
        path: &Path,
        filename: &str,
        caption: &str,
    ) -> Result<i32> {
        self.bot.send_chat_action(chat_id, ChatAction::UploadDocument).await?;
        let input_file = teloxide::types::InputFile::file(path).file_name(filename);
        let msg = self
            .bot
            .send_document(chat_id, input_file)
            .caption(caption)
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
        Ok(msg.id)
    }

    /// Send a text message to a chat. Returns message_id on success.
    pub async fn notify_with_text(
        &self,
        chat_id: ChatId,
        text: &str,
    ) -> Result<i32> {
        let msg = self
            .bot
            .send_message(chat_id, text)
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
        Ok(msg.id)
    }
}
```

- [ ] **Step 4: Register document module in `src/bot/notifier.rs`**

Add to the module declarations:
```rust
mod document;
```

- [ ] **Step 5: Add `build_eh_caption` to `src/utils/caption.rs`**

```rust
use eh_client::EhGallery;
use teloxide::utils::markdown;

/// Build a MarkdownV2 caption for an e-hentai gallery.
pub fn build_eh_caption(gallery: &EhGallery, base_url: &str) -> String {
    let mut lines = Vec::new();

    // Title
    lines.push(format!("📕 *{}*", markdown::escape(&gallery.title)));

    // Metadata line
    let rating_stars = "⭐".repeat((gallery.rating as u8).clamp(1, 5) as usize);
    let filesize_mb = gallery.filesize as f64 / 1_000_000.0;
    let meta = format!(
        "{} \\| {} \\| {}p \\| {:.1}MB",
        markdown::escape(&gallery.category),
        rating_stars,
        gallery.filecount,
        filesize_mb
    );
    lines.push(meta);

    // Tags (first 10)
    if !gallery.tags.is_empty() {
        let tag_str = gallery.tags
            .iter()
            .take(10)
            .map(|t| format!("#{}", markdown::escape(t)))
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(tag_str);
    }

    // Gallery URL
    let gallery_url = format!("{}/g/{}/{}/", base_url, gallery.gid, gallery.token);
    lines.push(format!("🔗 [{}]({})", markdown::escape("查看"), markdown::escape_link_url(&gallery_url)));

    lines.join("\n")
}
```

- [ ] **Step 6: Run `cargo check`**

Run: `cargo check`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add src/config.rs config.toml.example src/bot/notifier/ src/utils/caption.rs
git commit -m "feat: add EhentaiConfig, notifier document/text methods, eh caption builder"
```

---

## Task 8: EhEngine + EhDownloadProcessor

**Files:**
- Create: `src/scheduler/eh_engine.rs`
- Create: `src/scheduler/eh_download_processor.rs`
- Modify: `src/scheduler/mod.rs`
- Modify: `src/scheduler/helpers.rs`

- [ ] **Step 1: Add helper function to `src/scheduler/helpers.rs`**

```rust
use crate::db::types::EhTagState;

pub fn eh_tag_subscription_state(subscription: &subscriptions::Model) -> Option<EhTagState> {
    match &subscription.latest_data {
        Some(SubscriptionState::EhTag(state)) => Some(state.clone()),
        _ => None,
    }
}
```

- [ ] **Step 2: Create `src/scheduler/eh_engine.rs`**

```rust
use crate::config::EhentaiConfig;
use crate::db::repo::Repo;
use crate::db::types::{
    EhFilter, EhTagState, EhTaskKey, SubscriptionState, TaskType,
};
use anyhow::Result;
use chrono::Utc;
use eh_client::EhClient;
use std::sync::Arc;
use tokio::time::MissedTickBehavior;
use tracing::{error, info, warn};

const MAX_FETCH_PAGES: u32 = 5;
const MAX_METADATA_BATCH: usize = 25;

pub struct EhEngine {
    repo: Arc<Repo>,
    client: Arc<EhClient>,
    config: Arc<EhentaiConfig>,
    tick_interval_sec: u64,
}

impl EhEngine {
    pub fn new(
        repo: Arc<Repo>,
        client: Arc<EhClient>,
        config: Arc<EhentaiConfig>,
        tick_interval_sec: u64,
    ) -> Self {
        Self { repo, client, config, tick_interval_sec }
    }

    pub async fn run(self) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(
            self.tick_interval_sec.max(1),
        ));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            if let Err(e) = self.tick().await {
                error!("EhEngine tick error: {:#}", e);
            }
        }
    }

    async fn tick(&self) -> Result<()> {
        let tasks = self.repo.get_pending_tasks_by_type(TaskType::Ehentai, 1).await?;
        if tasks.is_empty() {
            return Ok(());
        }
        let task = &tasks[0];
        if let Err(e) = self.execute_eh_task(task).await {
            error!("EhEngine task {} error: {:#}", task.id, e);
            let backoff = Utc::now() + chrono::Duration::hours(1);
            let _ = self.repo.update_task_after_poll(task.id, backoff).await;
        }
        Ok(())
    }

    async fn execute_eh_task(&self, task: &crate::db::entities::tasks::Model) -> Result<()> {
        let key = EhTaskKey::parse(&task.value)
            .ok_or_else(|| anyhow::anyhow!("failed to parse eh task value: {}", task.value))?;

        let subs = self.repo.list_subscriptions_by_task(task.id).await?;
        if subs.is_empty() {
            self.schedule_next_poll(task.id);
            return Ok(());
        }

        // Compute aggregate filter across all subscriptions
        let eh_filters: Vec<EhFilter> = subs.iter()
            .filter_map(|s| s.eh_filter.clone())
            .collect();
        let agg_filter = if eh_filters.is_empty() {
            EhFilter::default()
        } else {
            let refs: Vec<&EhFilter> = eh_filters.iter().collect();
            EhFilter::aggregate(&refs)
        };

        // Determine scan mode
        let has_rating_filter = agg_filter.has_rating_filter();
        let cutoff_ts = if has_rating_filter {
            Utc::now().timestamp() - (self.config.scan_window_hours as i64 * 3600)
        } else {
            0 // not used in normal mode
        };

        // Compute oldest latest_posted_ts across subs (for normal mode)
        let oldest_ts = subs.iter()
            .filter_map(|s| {
                match &s.latest_data {
                    Some(SubscriptionState::EhTag(state)) => Some(state.latest_posted_ts),
                    _ => None,
                }
            })
            .min()
            .unwrap_or(0);

        // Search for galleries
        let mut all_refs = Vec::new();
        for page in 0..MAX_FETCH_PAGES {
            let refs = self.client.search(&key.query, key.category_bitmask, page).await?;
            if refs.is_empty() {
                break;
            }

            // Check if we should stop
            if has_rating_filter {
                // 48h scan: stop when oldest result is before cutoff
                if refs.iter().any(|r| r.posted_ts < cutoff_ts) {
                    all_refs.extend(refs.into_iter().filter(|r| r.posted_ts >= cutoff_ts));
                    break;
                }
            } else {
                // Normal mode: stop when we find galleries older than oldest_ts
                if refs.iter().any(|r| r.posted_ts <= oldest_ts) {
                    all_refs.extend(refs.into_iter().filter(|r| r.posted_ts > oldest_ts));
                    break;
                }
            }
            all_refs.extend(refs);
            // Rate limit between pages
            tokio::time::sleep(std::time::Duration::from_millis(3500)).await;
        }

        if all_refs.is_empty() {
            self.schedule_next_poll(task.id);
            return Ok(());
        }

        // Fetch metadata in batches of 25
        let mut all_galleries = Vec::new();
        for chunk in all_refs.chunks(MAX_METADATA_BATCH) {
            let gidlist: Vec<(u64, &str)> = chunk.iter()
                .map(|r| (r.gid, r.token.as_str()))
                .collect();
            let galleries = self.client.get_metadata(&gidlist).await?;
            all_galleries.extend(galleries);
            if all_refs.len() > MAX_METADATA_BATCH {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }

        // Filter galleries
        let filtered: Vec<_> = all_galleries.into_iter()
            .filter(|g| !g.expunged && agg_filter.matches(g))
            .collect();

        // For each subscription, enqueue downloads and update state
        let max_push = self.config.max_push_per_tick;
        let mut pushed_this_tick = 0;
        for sub in &subs {
            let eh_filter = sub.eh_filter.as_ref().unwrap_or(&agg_filter);
            let mut state = match &sub.latest_data {
                Some(SubscriptionState::EhTag(s)) => s.clone(),
                _ => EhTagState::cleared(0),
            };

            for gallery in &filtered {
                if pushed_this_tick >= max_push {
                    break;
                }
                if state.pushed_gids.contains(&gallery.gid) {
                    continue;
                }
                // Check per-subscription filter
                if !eh_filter.matches(gallery) {
                    continue;
                }

                // Enqueue download
                if let Err(e) = self.repo.enqueue_download(
                    sub.chat_id,
                    gallery.gid,
                    &gallery.token,
                    &gallery.title,
                    eh_filter.telegraph,
                    "subscription",
                ).await {
                    warn!("Failed to enqueue download for gid {}: {:#}", gallery.gid, e);
                    continue;
                }

                state.add_pushed_gid(gallery.gid);
                if gallery.posted > state.latest_posted_ts {
                    state.latest_posted_ts = gallery.posted;
                }
                pushed_this_tick += 1;
            }

            state.trim_pushed(500);
            let _ = self.repo.update_subscription_latest_data(
                sub.id,
                Some(SubscriptionState::EhTag(state)),
            ).await;
        }

        info!("EhEngine: enqueued {} galleries for task '{}'", pushed_this_tick, key.query);
        self.schedule_next_poll(task.id);
        Ok(())
    }

    fn schedule_next_poll(&self, task_id: i32) {
        let min = self.config.min_interval_sec;
        let max = self.config.max_interval_sec.max(min);
        let delay = if max > min {
            rand::rng().random_range(min..=max)
        } else {
            min
        };
        let next = Utc::now() + chrono::Duration::seconds(delay as i64);
        let repo = Arc::clone(&self.repo);
        tokio::spawn(async move {
            let _ = repo.update_task_after_poll(task_id, next).await;
        });
    }
}
```

- [ ] **Step 3: Create `src/scheduler/eh_download_processor.rs`**

```rust
use crate::bot::notifier::Notifier;
use crate::config::EhentaiConfig;
use crate::db::repo::Repo;
use crate::utils::caption;
use anyhow::Result;
use chrono::Utc;
use eh_client::{EhClient, TelegraphClient};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::time::MissedTickBehavior;
use tracing::{error, info, warn};

pub struct EhDownloadProcessor {
    repo: Arc<Repo>,
    notifier: Arc<Notifier>,
    client: Arc<EhClient>,
    telegraph: Option<Arc<TelegraphClient>>,
    config: Arc<EhentaiConfig>,
    poll_interval_sec: u64,
}

impl EhDownloadProcessor {
    pub fn new(
        repo: Arc<Repo>,
        notifier: Arc<Notifier>,
        client: Arc<EhClient>,
        telegraph: Option<Arc<TelegraphClient>>,
        config: Arc<EhentaiConfig>,
        poll_interval_sec: u64,
    ) -> Self {
        Self { repo, notifier, client, telegraph, config, poll_interval_sec }
    }

    pub async fn run(self) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(
            self.poll_interval_sec.max(10),
        ));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            if let Err(e) = self.process_one().await {
                error!("EhDownloadProcessor error: {:#}", e);
            }
        }
    }

    async fn process_one(&self) -> Result<()> {
        // 1. Crash recovery: reset stale "downloading" entries
        let reset_count = self.repo.reset_stale_downloading().await?;
        if reset_count > 0 {
            warn!("Reset {} stale downloads to pending", reset_count);
        }

        // 2. Check rate limit
        let window_start = Utc::now() - chrono::Duration::hours(self.config.download_rate_window_hours as i64);
        let downloaded_bytes = self.repo.get_downloaded_bytes_in_window(window_start).await?;
        let limit_bytes = self.config.download_rate_limit_gb * 1_000_000_000;
        let budget_remaining = limit_bytes.saturating_sub(downloaded_bytes);
        if budget_remaining == 0 {
            info!("Rate limit reached: {}GB used, waiting", downloaded_bytes / 1_000_000_000);
            return Ok(());
        }

        // 3. Get next pending download
        let download = match self.repo.get_next_pending_download().await? {
            Some(d) => d,
            None => return Ok(()), // queue empty
        };

        // 4. Check retry count
        if download.retry_count >= self.config.max_retry_count as i32 {
            let _ = self.repo.mark_download_permanently_failed(
                download.id,
                "max retries exceeded",
            ).await;
            return Ok(());
        }

        // 5. Mark as downloading
        self.repo.mark_download_started(download.id).await?;

        // 6. Fetch metadata for filesize check
        let gidlist = vec![(download.gid as u64, download.token.as_str())];
        let galleries = self.client.get_metadata(&gidlist).await?;
        let gallery = match galleries.into_iter().next() {
            Some(g) => g,
            None => {
                self.repo.mark_download_failed(download.id, "metadata not found").await?;
                return Ok(());
            }
        };

        // 7. Check if filesize exceeds budget
        if gallery.filesize > budget_remaining {
            info!("Skipping gid {} ({}MB > {}MB budget remaining)",
                download.gid, gallery.filesize / 1_000_000, budget_remaining / 1_000_000);
            // Reset to pending (will be retried when budget is available)
            self.repo.mark_download_failed(download.id, "rate limit: waiting for budget").await?;
            return Ok(());
        }

        // 8. Get archiver key and download archive
        let archiver_key = match self.client.get_archiver_key(gallery.gid, &gallery.token).await {
            Ok(k) => k,
            Err(e) => {
                self.repo.mark_download_failed(download.id, &format!("archiver_key: {e}")).await?;
                return Ok(());
            }
        };

        let tmp_dir = TempDir::new()?;
        let zip_path = tmp_dir.path().join(format!("{}.zip", download.gid));

        let actual_size = match self.client.download_archive(
            gallery.gid,
            &gallery.token,
            &archiver_key,
            &zip_path,
        ).await {
            Ok(size) => size,
            Err(e) => {
                self.repo.mark_download_failed(download.id, &format!("download: {e}")).await?;
                return Ok(());
            }
        };

        // 9. Send to chat
        let caption = caption::build_eh_caption(&gallery, self.config.base_url());
        let filename = format!("{}.zip", download.gid);

        if let Err(e) = self.notifier.notify_with_document(
            teloxide::types::ChatId(download.chat_id),
            &zip_path,
            &filename,
            &caption,
        ).await {
            self.repo.mark_download_failed(download.id, &format!("send: {e}")).await?;
            return Ok(());
        }

        // 10. Telegraph upload if enabled
        if download.telegraph {
            if let Some(ref telegraph) = self.telegraph {
                if let Err(e) = self.do_telegraph_upload(telegraph, &zip_path, &gallery).await {
                    warn!("Telegraph upload failed for gid {}: {:#}", download.gid, e);
                    // Don't fail the whole download — the ZIP was already sent
                }
            } else {
                warn!("Telegraph requested but no token configured for gid {}", download.gid);
            }
        }

        // 11. Mark as done
        self.repo.mark_download_done(download.id, actual_size).await?;
        info!("Completed download for gid {} ({}MB)", download.gid, actual_size / 1_000_000);

        Ok(())
    }

    async fn do_telegraph_upload(
        &self,
        telegraph: &TelegraphClient,
        zip_path: &std::path::Path,
        gallery: &eh_client::EhGallery,
    ) -> Result<()> {
        use std::io::Read;

        // Open ZIP and extract images
        let file = std::fs::File::open(zip_path)?;
        let mut archive = zip::ZipArchive::new(file)?;

        let mut image_urls = Vec::new();
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)?;
            let name = entry.name().to_lowercase();
            if name.ends_with(".jpg") || name.ends_with(".png")
                || name.ends_with(".gif") || name.ends_with(".webp")
            {
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf)?;
                let filename = std::path::Path::new(entry.name())
                    .file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or("image.jpg");
                match telegraph.upload_image(&buf, filename).await {
                    Ok(url) => image_urls.push(url),
                    Err(e) => warn!("Failed to upload image {}: {:#}", name, e),
                }
            }
        }

        if image_urls.is_empty() {
            return Ok(());
        }

        let page_url = telegraph.create_gallery_page(&gallery.title, &image_urls).await?;

        self.notifier.notify_with_text(
            teloxide::types::ChatId(0), // Will be set by caller context
            &format!("🔗 [{}]({})", "Telegraph", page_url),
        ).await?;

        Ok(())
    }
}
```

Note: The `do_telegraph_upload` method needs the correct chat_id. Since `download.chat_id` is available, pass it to `do_telegraph_upload`:

Fix the call:
```rust
if let Err(e) = self.do_telegraph_upload(telegraph, &zip_path, &gallery, download.chat_id).await {
```

And update the signature:
```rust
async fn do_telegraph_upload(
    &self,
    telegraph: &TelegraphClient,
    zip_path: &std::path::Path,
    gallery: &eh_client::EhGallery,
    chat_id: i64,
) -> Result<()> {
    // ... same body but:
    self.notifier.notify_with_text(
        teloxide::types::ChatId(chat_id),
        &format!("🔗 [{}]({})", "Telegraph", page_url),
    ).await?;
    Ok(())
}
```

- [ ] **Step 4: Register modules in `src/scheduler/mod.rs`**

Add:
```rust
mod eh_engine;
mod eh_download_processor;

pub use eh_engine::EhEngine;
pub use eh_download_processor::EhDownloadProcessor;
```

- [ ] **Step 5: Run `cargo check`**

Run: `cargo check`
Expected: PASS (may need minor fixes for imports)

- [ ] **Step 6: Commit**

```bash
git add src/scheduler/
git commit -m "feat: add EhEngine (search+enqueue) and EhDownloadProcessor (drain+send)"
```

---

## Task 9: Bot Commands + Handlers + Wiring

**Files:**
- Modify: `src/bot/commands.rs`
- Modify: `src/bot/handler.rs`
- Modify: `src/bot/mod.rs`
- Create: `src/bot/handlers/subscription/ehentai.rs`
- Create: `src/bot/handlers/subscription/eh_download.rs`
- Modify: `src/bot/handlers/subscription/mod.rs`
- Modify: `src/bot/handlers/subscription/helpers.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Add commands to `src/bot/commands.rs`**

Add to the `Command` enum (after `Cancel` or after booru commands):
```rust
    #[command(description = "订阅 e-hentai/exhentai 搜索")]
    ESub(String),
    #[command(description = "取消订阅 e-hentai/exhentai 搜索")]
    EUnsub(String),
    #[command(description = "列出 e-hentai/exhentai 订阅")]
    EList,
    #[command(description = "下载 e-hentai/exhentai 画廊")]
    EDl(String),
```

Update `user_commands`, `admin_commands`, `owner_commands` to accept `has_ehentai: bool` and extend with eh commands when `has_ehentai` is true:

```rust
pub fn user_commands(has_booru: bool, has_ehentai: bool) -> Vec<BotCommand> {
    let mut cmds = vec![
        BotCommand::new("start", "启动机器人"),
        BotCommand::new("help", "显示帮助信息"),
        BotCommand::new("sub", "订阅画师"),
        BotCommand::new("subrank", "订阅排行榜"),
        BotCommand::new("unsub", "取消订阅画师"),
        BotCommand::new("unsubrank", "取消订阅排行榜"),
        BotCommand::new("list", "列出活跃的订阅"),
        BotCommand::new("settings", "聊天设置"),
        BotCommand::new("cancel", "取消当前操作"),
        BotCommand::new("download", "下载原图"),
    ];
    if has_booru {
        cmds.extend_from_slice(&[
            BotCommand::new("bsub", "订阅 Booru 标签"),
            BotCommand::new("bunsub", "取消订阅 Booru 标签"),
            BotCommand::new("brand", "随机 Booru 图片"),
            BotCommand::new("brank", "Booru 排行"),
        ]);
    }
    if has_ehentai {
        cmds.extend_from_slice(&[
            BotCommand::new("esub", "订阅 E-Hentai 搜索"),
            BotCommand::new("eunsub", "取消订阅 E-Hentai"),
            BotCommand::new("elist", "列出 E-Hentai 订阅"),
            BotCommand::new("edl", "下载 E-Hentai 画廊"),
        ]);
    }
    cmds
}

pub fn admin_commands(has_booru: bool, has_ehentai: bool) -> Vec<BotCommand> {
    let mut cmds = vec![
        BotCommand::new("enablechat", "启用聊天"),
        BotCommand::new("disablechat", "禁用聊天"),
    ];
    if has_booru {
        cmds.push(BotCommand::new("blist", "列出 Booru 订阅"));
    }
    if has_ehentai {
        // admin can see eh list too
    }
    cmds
}

pub fn owner_commands(has_booru: bool, has_ehentai: bool) -> Vec<BotCommand> {
    let mut cmds = vec![
        BotCommand::new("setadmin", "设置管理员"),
        BotCommand::new("unsetadmin", "取消管理员"),
        BotCommand::new("info", "系统状态"),
    ];
    let _ = has_booru;
    let _ = has_ehentai;
    cmds
}
```

- [ ] **Step 2: Add dispatch to `src/bot/handler.rs`**

Add `eh_client: Option<Arc<EhClient>>` field to `BotHandler`. Update `new()` to accept it.

Add dispatch in `dispatch_command()`:
```rust
Command::ESub(args) => self.handle_esub(bot, chat_id, user_id, args).await,
Command::EUnsub(args) => self.handle_eunsub(bot, chat_id, user_id, args).await,
Command::EList => self.handle_elist(bot, chat_id, user_id).await,
Command::EDl(args) => self.handle_edl(bot, chat_id, user_id, args).await,
```

- [ ] **Step 3: Create `src/bot/handlers/subscription/ehentai.rs`**

```rust
use crate::bot::notifier::ThrottledBot;
use crate::bot::BotHandler;
use crate::db::types::{EhCategory, EhFilter, EhTaskKey, TagFilter, TaskType};
use crate::utils::args;
use teloxide::prelude::*;
use teloxide::types::{ChatId, ParseMode, UserId};
use teloxide::utils::markdown;
use tracing::error;

impl BotHandler {
    pub async fn handle_esub(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        _user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        if self.eh_client.is_none() {
            bot.send_message(chat_id, "E-Hentai 功能未启用")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        let parsed = args::parse_args(&args_str);
        let target = self.resolve_subscription_target(chat_id, &parsed).await?;
        let remaining = parsed.remaining.trim();
        if remaining.is_empty() {
            bot.send_message(chat_id, "用法: `/esub <query> [rating>=N] [pages>=N] [cat=...] [telegraph=on]`")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        let parts: Vec<&str> = remaining.split_whitespace().collect();
        let mut query_parts = Vec::new();
        let mut eh_filter = EhFilter::new();
        let mut category_bitmask = 0u32;

        for part in &parts {
            if let Some(val) = part.strip_prefix("rating>=") {
                eh_filter.min_rating = val.parse().ok();
            } else if let Some(val) = part.strip_prefix("rating>") {
                eh_filter.min_rating = val.parse::<u8>().ok().map(|v| v.saturating_add(1));
            } else if let Some(val) = part.strip_prefix("pages>=") {
                eh_filter.min_pages = val.parse().ok();
            } else if let Some(val) = part.strip_prefix("pages>") {
                eh_filter.min_pages = val.parse::<u32>().ok().map(|v| v.saturating_add(1));
            } else if let Some(val) = part.strip_prefix("pages<=") {
                eh_filter.max_pages = val.parse().ok();
            } else if let Some(val) = part.strip_prefix("pages<") {
                eh_filter.max_pages = val.parse::<u32>().ok().map(|v| v.saturating_sub(1));
            } else if let Some(val) = part.strip_prefix("cat=") {
                category_bitmask = EhCategory::bitmask_from_str(val);
            } else if let Some(val) = part.strip_prefix("telegraph=") {
                eh_filter.telegraph = val.eq_ignore_ascii_case("on");
            } else {
                query_parts.push(*part);
            }
        }

        let query = query_parts.join(" ");
        if query.is_empty() {
            bot.send_message(chat_id, "请提供搜索关键词")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        let task_key = EhTaskKey::new(&query, category_bitmask, &eh_filter);
        let task_value = task_key.to_task_value();

        match self.create_eh_subscription(target.chat_id, TaskType::Ehentai, &task_value, &query, TagFilter::default(), &eh_filter).await {
            Ok(()) => {
                let filter_desc = if eh_filter.is_empty() {
                    "无过滤".to_string()
                } else {
                    eh_filter.format_for_display()
                };
                let msg = format!(
                    "✅ 已订阅 E-Hentai: `{}`\n过滤器: {}\n搜索语法请参考 e-hentai 官网",
                    markdown::escape(&query),
                    markdown::escape(&filter_desc),
                );
                bot.send_message(chat_id, msg)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                error!("Failed to create eh subscription: {:#}", e);
                bot.send_message(chat_id, "订阅失败，请稍后重试")
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
        }
        Ok(())
    }

    pub async fn handle_eunsub(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        _user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        let parsed = args::parse_args(&args_str);
        let target = self.resolve_subscription_target(chat_id, &parsed).await?;
        let query = parsed.remaining.trim().to_string();

        if query.is_empty() {
            bot.send_message(chat_id, "用法: `/eunsub <query>`")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        // If query contains '|', treat as internal key
        let task_value = if query.contains('|') {
            // Prepend "eh:" if not present
            if query.starts_with("eh:") {
                query
            } else {
                format!("eh:{}", query)
            }
        } else {
            format!("eh:{}", query)
        };

        match self.delete_subscription(target.chat_id, TaskType::Ehentai, &task_value).await {
            Ok(Some(_)) => {
                bot.send_message(chat_id, "✅ 已取消订阅")
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Ok(None) => {
                bot.send_message(chat_id, "未找到对应的订阅")
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                error!("Failed to delete eh subscription: {:#}", e);
                bot.send_message(chat_id, "取消订阅失败")
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
        }
        Ok(())
    }

    pub async fn handle_elist(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        _user_id: Option<UserId>,
    ) -> ResponseResult<()> {
        let subs = self.repo.list_subscriptions_by_chat(chat_id).await;
        let subs = match subs {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to list eh subscriptions: {:#}", e);
                bot.send_message(chat_id, "获取订阅列表失败")
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
                return Ok(());
            }
        };

        let eh_subs: Vec<_> = subs.into_iter()
            .filter(|(sub, task)| task.r#type == TaskType::Ehentai)
            .collect();

        if eh_subs.is_empty() {
            bot.send_message(chat_id, "没有 E-Hentai 订阅")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        let mut lines = vec!["📋 *E-Hentai 订阅列表*".to_string()];
        for (i, (sub, task)) in eh_subs.iter().enumerate() {
            let key = EhTaskKey::parse(&task.value);
            if let Some(k) = key {
                let filter = sub.eh_filter.as_ref()
                    .map(|f| f.format_for_display())
                    .unwrap_or_else(|| "无".to_string());
                lines.push(format!(
                    "\n{}\\. `{}`\n   过滤: {}",
                    i + 1,
                    markdown::escape(&k.query),
                    markdown::escape(&filter),
                ));
            }
        }

        let msg = lines.join("\n");
        bot.send_message(chat_id, msg)
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
        Ok(())
    }
}
```

- [ ] **Step 4: Create `src/bot/handlers/subscription/eh_download.rs`**

```rust
use crate::bot::notifier::ThrottledBot;
use crate::bot::BotHandler;
use crate::db::types::TaskType;
use crate::utils::args;
use regex::Regex;
use teloxide::prelude::*;
use teloxide::types::{ChatId, ParseMode, UserId};
use teloxide::utils::markdown;
use tracing::error;
use std::sync::OnceLock;

fn gallery_url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"https?://(?:e-hentai|exhentai)\.org/g/(\d+)/([0-9a-f]+)/?")
            .expect("invalid gallery url regex")
    })
}

impl BotHandler {
    pub async fn handle_edl(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        _user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        if self.eh_client.is_none() {
            bot.send_message(chat_id, "E-Hentai 功能未启用")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        let parsed = args::parse_args(&args_str);
        let target = self.resolve_subscription_target(chat_id, &parsed).await?;
        let remaining = parsed.remaining.trim();
        let telegraph = parsed.has("telegraph") &&
            parsed.get("telegraph").map(|v| v.eq_ignore_ascii_case("on")).unwrap_or(false);

        if remaining.is_empty() {
            bot.send_message(chat_id, "用法: `/edl <url|gid/token> [telegraph=on]`")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        // Parse gallery URL or gid/token
        let (gid, token) = if let Some(caps) = gallery_url_re().captures(remaining) {
            let g: u64 = caps[1].parse().unwrap_or(0);
            (g, caps[2].to_string())
        } else if let Some(slash) = remaining.find('/') {
            let g: u64 = remaining[..slash].parse().unwrap_or(0);
            (g, remaining[slash + 1..].trim().to_string())
        } else {
            bot.send_message(chat_id, "无法解析画廊 URL 或 gid/token")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        };

        if gid == 0 || token.is_empty() {
            bot.send_message(chat_id, "无效的画廊 ID")
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        let client = self.eh_client.as_ref().unwrap();

        // Fetch metadata for title
        let gidlist = vec![(gid, token.as_str())];
        let galleries = match client.get_metadata(&gidlist).await {
            Ok(g) => g,
            Err(e) => {
                error!("Failed to fetch gallery metadata: {:#}", e);
                bot.send_message(chat_id, "获取画廊信息失败")
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
                return Ok(());
            }
        };

        let gallery = match galleries.into_iter().next() {
            Some(g) => g,
            None => {
                bot.send_message(chat_id, "画廊不存在或已被删除")
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
                return Ok(());
            }
        };

        // Enqueue download
        match self.repo.enqueue_download(
            target.chat_id,
            gid,
            &token,
            &gallery.title,
            telegraph,
            "direct",
        ).await {
            Ok(()) => {
                let pending = self.repo.count_pending_downloads().await.unwrap_or(0);
                let size_mb = gallery.filesize as f64 / 1_000_000.0;
                let msg = format!(
                    "✅ 已加入下载队列\n\n标题: `{}`\n大小: {:.1}MB\n队列位置: {}",
                    markdown::escape(&gallery.title),
                    size_mb,
                    pending,
                );
                bot.send_message(chat_id, msg)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                error!("Failed to enqueue download: {:#}", e);
                bot.send_message(chat_id, "加入下载队列失败")
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
        }
        Ok(())
    }
}
```

- [ ] **Step 5: Register modules in `src/bot/handlers/subscription/mod.rs`**

Add:
```rust
mod ehentai;
mod eh_download;
```

- [ ] **Step 6: Add `create_eh_subscription` helper to `src/bot/handlers/subscription/helpers.rs`**

```rust
pub async fn create_eh_subscription(
    &self,
    chat_id: i64,
    task_type: TaskType,
    task_value: &str,
    display_name: &str,
    filter_tags: TagFilter,
    eh_filter: &EhFilter,
) -> Result<()> {
    let task = self.repo.get_or_create_task(
        task_type,
        task_value.to_string(),
        Some(display_name.to_string()),
    ).await?;

    let eh_filter_opt = if eh_filter.is_empty() {
        None
    } else {
        Some(eh_filter.clone())
    };

    self.repo.upsert_eh_subscription(
        chat_id,
        task.id,
        filter_tags,
        eh_filter_opt,
    ).await?;
    Ok(())
}
```

- [ ] **Step 7: Update `src/bot/mod.rs`**

Add `has_ehentai` parameter to `setup_commands`. Pass `eh_client: Option<Arc<EhClient>>` to `BotHandler::new()`. Update `bot::run()` signature to accept `eh_client`.

```rust
pub async fn run(
    bot: Bot,
    telegram_config: TelegramConfig,
    repo: Arc<Repo>,
    pixiv_client: Arc<RwLock<PixivClient>>,
    notifier: Arc<Notifier>,
    sensitive_tags: Tags,
    image_size: ImageSize,
    download_original_threshold: u8,
    cache_dir: String,
    log_dir: String,
    booru_registry: Option<Arc<BooruSiteRegistry>>,
    eh_client: Option<Arc<EhClient>>,
) -> anyhow::Result<()> {
    let has_booru = booru_registry.is_some();
    let has_ehentai = eh_client.is_some();

    let handler = BotHandler::new(
        Arc::clone(&repo),
        pixiv_client,
        Arc::clone(&notifier),
        sensitive_tags,
        telegram_config.owner_id,
        telegram_config.bot_mode.is_public(),
        image_size,
        download_original_threshold,
        telegram_config.require_mention_in_group,
        cache_dir,
        log_dir,
        booru_registry,
        eh_client,
    );

    setup_commands(&bot, &repo, has_booru, has_ehentai).await?;
    // ... rest of existing code
}
```

- [ ] **Step 8: Update `src/main.rs`**

Build EhClient and spawn engines:

```rust
// After booru setup:
use eh_client::{EhClient, EhCookies, TelegraphClient};
use pixivbot::scheduler::{EhEngine, EhDownloadProcessor};

let eh_cookies = EhCookies {
    ipb_member_id: config.ehentai.ipb_member_id.clone(),
    ipb_pass_hash: config.ehentai.ipb_pass_hash.clone(),
    igneous: config.ehentai.igneous.clone(),
    nw: true,
};

let eh_client = if config.ehentai.is_enabled() {
    if config.ehentai.site == "exhentai" && !config.ehentai.is_exhentai_ready() {
        tracing::warn!("ExHentai enabled but missing required cookies. EH feature disabled.");
        None
    } else {
        match EhClient::new(
            config.ehentai.base_url(),
            config.ehentai.api_url(),
            eh_cookies,
            &config.ehentai.image_resolution,
        ) {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => {
                tracing::error!("Failed to create EhClient: {:#}", e);
                None
            }
        }
    }
} else {
    None
};

let telegraph_client = config.ehentai.telegraph_access_token.as_ref()
    .map(|token| Arc::new(TelegraphClient::new(token.clone())));

if let Some(ref client) = eh_client {
    let eh_engine = EhEngine::new(
        Arc::clone(&repo),
        Arc::clone(client),
        Arc::new(config.ehentai.clone()),
        config.scheduler.tick_interval_sec,
    );
    let handle = tokio::spawn(async move { eh_engine.run().await });
    task_handles.push(handle);

    let eh_processor = EhDownloadProcessor::new(
        Arc::clone(&repo),
        Arc::clone(&notifier),
        Arc::clone(client),
        telegraph_client.clone(),
        Arc::new(config.ehentai.clone()),
        config.ehentai.download_poll_interval_sec,
    );
    let handle = tokio::spawn(async move { eh_processor.run().await });
    task_handles.push(handle);
}
```

Pass `eh_client` to `bot::run(...)`.

- [ ] **Step 9: Run `cargo check`**

Run: `cargo check`
Expected: PASS (may need minor import fixes)

- [ ] **Step 10: Run `cargo fmt`**

Run: `cargo fmt`
Expected: PASS

- [ ] **Step 11: Run clippy**

Run: `cargo clippy -- -D warnings`
Expected: PASS (fix any warnings)

- [ ] **Step 12: Commit**

```bash
git add src/bot/ src/main.rs
git commit -m "feat: add ehentai bot commands, handlers, and main.rs wiring"
```

---

## Task 10: CI Verification + config.toml.example Update

**Files:**
- Verify: all files compile, tests pass
- Modify: `config.toml.example` (if not done in Task 7)

- [ ] **Step 1: Run full CI check**

Run: `make ci`
Expected: PASS (fmt-check → clippy → check → test → release build)

If any step fails:
- Fix the issue in the relevant file
- Re-run `make ci` until it passes

- [ ] **Step 2: Verify config.toml.example is updated**

Ensure the `[ehentai]` section is present in `config.toml.example` (should have been added in Task 7).

- [ ] **Step 3: Run quick test**

Run: `make quick`
Expected: PASS

- [ ] **Step 4: Run focused tests**

Run: `cargo test -p eh_client`
Run: `cargo test -p pixivbot -- eh_filter eh_task_key eh_tag_state`
Expected: all tests PASS

- [ ] **Step 5: Final commit**

```bash
git add -A
git commit -m "chore: fix CI issues and finalize ehentai feature"
```
