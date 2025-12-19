//! E-Hentai/ExHentai API 客户端
//!
//! 参考实现: https://github.com/lolishinshi/exloli-next/blob/master/src/ehentai/client.rs

use crate::error::{Error, Result};
use crate::models::*;
use reqwest::cookie::Jar;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, USER_AGENT};
use std::sync::Arc;
use std::time::Duration;

const EHENTAI_HOST: &str = "https://e-hentai.org";
const EXHENTAI_HOST: &str = "https://exhentai.org";
const API_URL: &str = "https://api.e-hentai.org/api.php";

const USER_AGENT_VALUE: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/120.0.0.0 Safari/537.36";

/// E-Hentai 源选择
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EhSource {
    /// e-hentai.org (不需要登录)
    #[default]
    EHentai,
    /// exhentai.org (需要登录)
    ExHentai,
}

impl EhSource {
    pub fn host(&self) -> &'static str {
        match self {
            EhSource::EHentai => EHENTAI_HOST,
            EhSource::ExHentai => EXHENTAI_HOST,
        }
    }
}

/// 认证信息
#[derive(Debug, Clone)]
pub struct EhCredentials {
    /// ipb_member_id cookie
    pub member_id: String,
    /// ipb_pass_hash cookie
    pub pass_hash: String,
    /// igneous cookie (optional, for exhentai)
    pub igneous: Option<String>,
}

/// E-Hentai 客户端配置
#[derive(Debug, Clone)]
pub struct EhClientConfig {
    /// 使用的源
    pub source: EhSource,
    /// 登录凭据 (可选, exhentai 必须)
    pub credentials: Option<EhCredentials>,
    /// 请求超时时间 (秒)
    pub timeout_secs: u64,
}

impl Default for EhClientConfig {
    fn default() -> Self {
        Self {
            source: EhSource::EHentai,
            credentials: None,
            timeout_secs: 30,
        }
    }
}

/// E-Hentai API 客户端
pub struct EhClient {
    client: reqwest::Client,
    config: EhClientConfig,
}

impl EhClient {
    /// 创建新的客户端
    pub fn new(config: EhClientConfig) -> Result<Self> {
        // Validate: ExHentai requires credentials
        if config.source == EhSource::ExHentai && config.credentials.is_none() {
            return Err(Error::Auth(
                "ExHentai requires login credentials".to_string(),
            ));
        }

        // Build cookie jar with credentials
        let jar = Arc::new(Jar::default());

        if let Some(ref creds) = config.credentials {
            // Add cookies for both domains
            for domain in &["https://e-hentai.org", "https://exhentai.org"] {
                let member_cookie = format!(
                    "ipb_member_id={}; Domain={}",
                    creds.member_id,
                    domain.replace("https://", "")
                );
                let pass_cookie = format!(
                    "ipb_pass_hash={}; Domain={}",
                    creds.pass_hash,
                    domain.replace("https://", "")
                );

                jar.add_cookie_str(&member_cookie, &domain.parse().unwrap());
                jar.add_cookie_str(&pass_cookie, &domain.parse().unwrap());

                if let Some(ref igneous) = creds.igneous {
                    let igneous_cookie = format!(
                        "igneous={}; Domain={}",
                        igneous,
                        domain.replace("https://", "")
                    );
                    jar.add_cookie_str(&igneous_cookie, &domain.parse().unwrap());
                }
            }
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .cookie_provider(jar)
            .build()?;

        Ok(Self { client, config })
    }

    /// 构建请求头
    fn build_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("text/html,application/json,*/*;q=0.8"),
        );
        headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));
        headers
    }

    /// 获取画廊元数据 (通过 API)
    ///
    /// # 参数
    /// - `gidlist`: 画廊 ID 和 Token 列表 [(gid, token), ...]
    pub async fn get_gallery_metadata(
        &self,
        gidlist: &[(u64, &str)],
    ) -> Result<Vec<GalleryMetadata>> {
        if gidlist.is_empty() {
            return Ok(vec![]);
        }

        // Build API request
        let gidlist_json: Vec<[serde_json::Value; 2]> = gidlist
            .iter()
            .map(|(gid, token)| {
                [
                    serde_json::Value::Number((*gid).into()),
                    serde_json::Value::String(token.to_string()),
                ]
            })
            .collect();

        let request_body = serde_json::json!({
            "method": "gdata",
            "gidlist": gidlist_json,
            "namespace": 1
        });

        let response = self
            .client
            .post(API_URL)
            .headers(self.build_headers())
            .json(&request_body)
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await?;

        if !status.is_success() {
            return Err(Error::Api {
                message: text,
                status: status.as_u16(),
            });
        }

        let parsed: GalleryMetadataResponse = serde_json::from_str(&text)?;
        Ok(parsed.gmetadata)
    }

    /// 获取单个画廊的元数据
    pub async fn get_gallery(&self, gid: u64, token: &str) -> Result<GalleryMetadata> {
        let results = self.get_gallery_metadata(&[(gid, token)]).await?;
        results
            .into_iter()
            .next()
            .ok_or_else(|| Error::GalleryNotFound(format!("{}/{}", gid, token)))
    }

    /// 搜索画廊
    ///
    /// # 参数
    /// - `query`: 搜索关键词
    /// - `categories`: 分类过滤 (None 表示全部)
    /// - `min_rating`: 最低评分过滤 (2-5)
    /// - `page`: 页码 (从 0 开始)
    pub async fn search(
        &self,
        query: &str,
        categories: Option<&[Category]>,
        min_rating: Option<u8>,
        page: u32,
    ) -> Result<SearchResult> {
        let host = self.config.source.host();

        // Build category filter
        let cat_filter = if let Some(cats) = categories {
            // Calculate category bitmask (each category has a specific value)
            let mut mask = 0u32;
            for cat in cats {
                mask |= category_bitmask(*cat);
            }
            // E-Hentai uses inverse: f_cats is what to EXCLUDE
            // So we invert: 1023 (all) XOR selected = excluded
            1023 ^ mask
        } else {
            0 // No filter, show all
        };

        let mut url = format!("{}/?f_search={}", host, urlencoding::encode(query));

        if cat_filter > 0 {
            url.push_str(&format!("&f_cats={}", cat_filter));
        }

        if let Some(rating) = min_rating {
            if (2..=5).contains(&rating) {
                url.push_str(&format!("&f_srdd={}", rating));
            }
        }

        if page > 0 {
            url.push_str(&format!("&page={}", page));
        }

        tracing::debug!("Searching: {}", url);

        let response = self
            .client
            .get(&url)
            .headers(self.build_headers())
            .send()
            .await?;

        let status = response.status();
        let html = response.text().await?;

        if !status.is_success() {
            return Err(Error::Api {
                message: html,
                status: status.as_u16(),
            });
        }

        // Check for sad panda (exhentai access denied)
        if html.contains("sadpanda.jpg") || html.len() < 1000 {
            return Err(Error::ExhentaiRequired);
        }

        // Parse search results from HTML
        self.parse_search_results(&html, page)
    }

    /// 从 HTML 解析搜索结果
    fn parse_search_results(&self, html: &str, page: u32) -> Result<SearchResult> {
        let mut galleries = Vec::new();
        let mut gid_tokens = Vec::new();

        // Simple regex-based parsing for gallery links
        // Format: /g/{gid}/{token}/
        let gallery_regex =
            regex::Regex::new(r#"href="https://e[x-]?hentai\.org/g/(\d+)/([a-f0-9]+)/""#).unwrap();

        for cap in gallery_regex.captures_iter(html) {
            if let (Some(gid_str), Some(token)) = (cap.get(1), cap.get(2)) {
                if let Ok(gid) = gid_str.as_str().parse::<u64>() {
                    let token_str = token.as_str().to_string();
                    // Avoid duplicates
                    if !gid_tokens.iter().any(|(g, _): &(u64, String)| *g == gid) {
                        gid_tokens.push((gid, token_str));
                    }
                }
            }
        }

        // Fetch metadata for all found galleries
        if !gid_tokens.is_empty() {
            // Convert to the format expected by get_gallery_metadata
            let gidlist: Vec<(u64, &str)> =
                gid_tokens.iter().map(|(g, t)| (*g, t.as_str())).collect();

            // Limit to 25 per API call
            for chunk in gidlist.chunks(25) {
                match tokio::runtime::Handle::current()
                    .block_on(async { self.get_gallery_metadata(chunk).await })
                {
                    Ok(metas) => galleries.extend(metas),
                    Err(e) => {
                        tracing::warn!("Failed to fetch gallery metadata: {}", e);
                    }
                }
            }
        }

        // Check if there are more pages (look for "next" link)
        let has_next = html.contains(">Next<") || html.contains(&format!("page={}", page + 1));

        Ok(SearchResult {
            galleries,
            has_next,
            page,
            total: None,
        })
    }

    /// 异步解析搜索结果
    async fn parse_search_results_async(&self, html: &str, page: u32) -> Result<SearchResult> {
        let mut gid_tokens = Vec::new();

        // Simple regex-based parsing for gallery links
        let gallery_regex =
            regex::Regex::new(r#"href="https://e[x-]?hentai\.org/g/(\d+)/([a-f0-9]+)/""#).unwrap();

        for cap in gallery_regex.captures_iter(html) {
            if let (Some(gid_str), Some(token)) = (cap.get(1), cap.get(2)) {
                if let Ok(gid) = gid_str.as_str().parse::<u64>() {
                    let token_str = token.as_str().to_string();
                    if !gid_tokens.iter().any(|(g, _): &(u64, String)| *g == gid) {
                        gid_tokens.push((gid, token_str));
                    }
                }
            }
        }

        let mut galleries = Vec::new();

        // Fetch metadata for all found galleries
        if !gid_tokens.is_empty() {
            let gidlist: Vec<(u64, &str)> =
                gid_tokens.iter().map(|(g, t)| (*g, t.as_str())).collect();

            for chunk in gidlist.chunks(25) {
                match self.get_gallery_metadata(chunk).await {
                    Ok(metas) => galleries.extend(metas),
                    Err(e) => {
                        tracing::warn!("Failed to fetch gallery metadata: {}", e);
                    }
                }
            }
        }

        let has_next = html.contains(">Next<") || html.contains(&format!("page={}", page + 1));

        Ok(SearchResult {
            galleries,
            has_next,
            page,
            total: None,
        })
    }

    /// 搜索画廊 (异步版本)
    pub async fn search_async(
        &self,
        query: &str,
        categories: Option<&[Category]>,
        min_rating: Option<u8>,
        page: u32,
    ) -> Result<SearchResult> {
        let host = self.config.source.host();

        let cat_filter = if let Some(cats) = categories {
            let mut mask = 0u32;
            for cat in cats {
                mask |= category_bitmask(*cat);
            }
            1023 ^ mask
        } else {
            0
        };

        let mut url = format!("{}/?f_search={}", host, urlencoding::encode(query));

        if cat_filter > 0 {
            url.push_str(&format!("&f_cats={}", cat_filter));
        }

        if let Some(rating) = min_rating {
            if (2..=5).contains(&rating) {
                url.push_str(&format!("&f_srdd={}", rating));
            }
        }

        if page > 0 {
            url.push_str(&format!("&page={}", page));
        }

        tracing::debug!("Searching: {}", url);

        let response = self
            .client
            .get(&url)
            .headers(self.build_headers())
            .send()
            .await?;

        let status = response.status();
        let html = response.text().await?;

        if !status.is_success() {
            return Err(Error::Api {
                message: html,
                status: status.as_u16(),
            });
        }

        if html.contains("sadpanda.jpg") || html.len() < 1000 {
            return Err(Error::ExhentaiRequired);
        }

        self.parse_search_results_async(&html, page).await
    }

    /// 检查画廊是否有更新版本
    ///
    /// 返回最新版本的 (gid, token) 如果存在更新，否则返回 None
    pub async fn check_gallery_update(
        &self,
        gid: u64,
        token: &str,
    ) -> Result<Option<(u64, String)>> {
        let meta = self.get_gallery(gid, token).await?;

        // Check if there's a parent (newer version)
        // In E-Hentai, parent_gid points to the NEWEST version
        if let (Some(parent_gid_str), Some(parent_key)) = (&meta.parent_gid, &meta.parent_key) {
            if let Ok(parent_gid) = parent_gid_str.parse::<u64>() {
                if parent_gid != gid {
                    return Ok(Some((parent_gid, parent_key.clone())));
                }
            }
        }

        Ok(None)
    }

    /// 获取画廊页面图片列表
    ///
    /// 注意: 这需要解析 HTML 页面，可能受到配额限制
    pub async fn get_gallery_pages(&self, gid: u64, token: &str, page: u32) -> Result<Vec<String>> {
        let host = self.config.source.host();
        let url = format!("{}/g/{}/{}/?p={}", host, gid, token, page);

        let response = self
            .client
            .get(&url)
            .headers(self.build_headers())
            .send()
            .await?;

        let status = response.status();
        let html = response.text().await?;

        if !status.is_success() {
            return Err(Error::Api {
                message: html,
                status: status.as_u16(),
            });
        }

        if html.contains("sadpanda.jpg") || html.len() < 1000 {
            return Err(Error::ExhentaiRequired);
        }

        // Parse page URLs from HTML
        // Looking for links like: href="https://e-hentai.org/s/{hash}/{gid}-{page}"
        let page_regex =
            regex::Regex::new(r#"href="(https://e[x-]?hentai\.org/s/[a-f0-9]+/\d+-\d+)""#).unwrap();

        let urls: Vec<String> = page_regex
            .captures_iter(&html)
            .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_string()))
            .collect();

        Ok(urls)
    }

    /// 检查是否已登录
    pub fn is_authenticated(&self) -> bool {
        self.config.credentials.is_some()
    }

    /// 获取当前源
    pub fn source(&self) -> EhSource {
        self.config.source
    }
}

/// 获取分类的位掩码值
fn category_bitmask(cat: Category) -> u32 {
    match cat {
        Category::Doujinshi => 2,
        Category::Manga => 4,
        Category::ArtistCg => 8,
        Category::GameCg => 16,
        Category::Western => 512,
        Category::NonH => 256,
        Category::ImageSet => 32,
        Category::Cosplay => 64,
        Category::AsianPorn => 128,
        Category::Misc => 1,
        Category::Unknown => 0,
    }
}

/// URL 编码模块
mod urlencoding {
    pub fn encode(s: &str) -> String {
        url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
    }
}
