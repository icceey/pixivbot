//! Pixiv API 客户端实现

use crate::auth;
use crate::error::{Error, Result};
use crate::models::*;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, USER_AGENT};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

const APP_API_HOST: &str = "https://app-api.pixiv.net";
const USER_AGENT_VALUE: &str = "PixivIOSApp/7.13.3 (iOS 14.6; iPhone13,2)";

/// Token 信息，包含 access_token 和过期时间
#[derive(Debug, Clone)]
struct TokenInfo {
    access_token: String,
    /// Token 过期的时间点
    expires_at: Instant,
}

impl TokenInfo {
    /// 检查 token 是否已过期（提前 60 秒刷新，避免边界情况）
    fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at - Duration::from_secs(60)
    }
}

/// Pixiv API 客户端
pub struct PixivClient {
    client: reqwest::Client,
    token_info: RwLock<Option<TokenInfo>>,
    refresh_token: String,
}

impl PixivClient {
    /// 创建新的客户端
    pub fn new(refresh_token: String) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        Ok(Self {
            client,
            token_info: RwLock::new(None),
            refresh_token,
        })
    }

    /// 使用 refresh_token 进行认证
    pub async fn login(&self) -> Result<()> {
        let auth_response =
            auth::auth_with_refresh_token(&self.client, &self.refresh_token).await?;

        // 计算过期时间点
        let expires_at = Instant::now() + Duration::from_secs(auth_response.expires_in);

        let mut token_info = self.token_info.write().await;
        *token_info = Some(TokenInfo {
            access_token: auth_response.access_token,
            expires_at,
        });

        tracing::info!(
            "Token refreshed, expires in {} seconds",
            auth_response.expires_in
        );

        Ok(())
    }

    /// 确保 token 有效，如果过期则自动刷新
    async fn ensure_token_valid(&self) -> Result<()> {
        let needs_refresh = {
            let token_info = self.token_info.read().await;
            match &*token_info {
                Some(info) => info.is_expired(),
                None => true, // 从未登录过
            }
        };

        if needs_refresh {
            self.login().await?;
        }

        Ok(())
    }

    /// 构建请求头
    async fn build_headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();

        // 设置 User-Agent
        headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
        headers.insert("App-OS", HeaderValue::from_static("ios"));
        headers.insert("App-OS-Version", HeaderValue::from_static("14.6"));

        // 设置 Authorization
        let token_info = self.token_info.read().await;
        if let Some(ref info) = *token_info {
            let auth_value = format!("Bearer {}", info.access_token);
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&auth_value)
                    .map_err(|e| Error::Other(format!("Invalid auth header: {}", e)))?,
            );
        } else {
            return Err(Error::Auth(
                "Not authenticated, call login() first".to_string(),
            ));
        }

        Ok(headers)
    }

    /// GET 请求
    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<T> {
        // 确保 token 有效，必要时自动刷新
        self.ensure_token_valid().await?;

        let url = format!("{}{}", APP_API_HOST, path);
        let headers = self.build_headers().await?;

        let response = self
            .client
            .get(&url)
            .headers(headers)
            .query(params)
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

        let result: T = serde_json::from_str(&text)?;
        Ok(result)
    }

    async fn anonymous_get_text(&self, url: reqwest::Url) -> Result<String> {
        let response = self
            .client
            .get(url)
            // pixivision serves different article markup to the authenticated iOS app's user agent.
            .header(USER_AGENT, concat!("PixivBot/", env!("CARGO_PKG_VERSION")))
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
        Ok(text)
    }

    /// 获取用户作品列表
    ///
    /// # 参数
    /// - `user_id`: 画师 ID
    /// - `illust_type`: 作品类型 ("illust", "manga" 或 None 表示全部)
    /// - `offset`: 分页偏移量
    pub async fn user_illusts(
        &self,
        user_id: u64,
        illust_type: Option<&str>,
        offset: Option<u32>,
    ) -> Result<UserIllusts> {
        let mut params = vec![
            ("user_id", user_id.to_string()),
            ("filter", "for_ios".to_string()),
        ];

        if let Some(t) = illust_type {
            params.push(("type", t.to_string()));
        }

        if let Some(o) = offset {
            params.push(("offset", o.to_string()));
        }

        self.get("/v1/user/illusts", &params).await
    }

    /// 获取用户公开收藏，使用 `max_bookmark_id` 游标分页。
    pub async fn user_bookmarks_illust(
        &self,
        user_id: u64,
        max_bookmark_id: Option<u64>,
    ) -> Result<UserIllusts> {
        let mut params = vec![
            ("user_id", user_id.to_string()),
            ("restrict", "public".to_string()),
            ("filter", "for_ios".to_string()),
        ];
        if let Some(cursor) = max_bookmark_id {
            params.push(("max_bookmark_id", cursor.to_string()));
        }
        self.get("/v1/user/bookmarks/illust", &params).await
    }

    /// 获取官方合集中的作品 ID，按页面视觉位置排序。
    pub async fn collection_work_ids(&self, collection_id: &str) -> Result<Vec<u64>> {
        if collection_id.is_empty() || !collection_id.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(Error::Other("Invalid Pixiv collection ID".to_string()));
        }
        let url = reqwest::Url::parse(&format!(
            "https://www.pixiv.net/ajax/collection/{collection_id}"
        ))
        .map_err(|error| Error::Other(format!("Invalid Pixiv collection URL: {error}")))?;
        let text = self.anonymous_get_text(url).await?;
        crate::gallery::collection_work_ids(&text)
    }

    /// 获取作者主页中全部插画和漫画作品 ID。
    pub async fn author_work_ids(&self, user_id: u64) -> Result<Vec<u64>> {
        let url = reqwest::Url::parse(&format!(
            "https://www.pixiv.net/ajax/user/{user_id}/profile/all"
        ))
        .map_err(|error| Error::Other(format!("Invalid Pixiv author URL: {error}")))?;
        let text = self.anonymous_get_text(url).await?;
        crate::gallery::author_work_ids(&text)
    }

    /// 单页作品按正文顺序返回，跨页去重由遍历者负责。
    pub async fn pixivision_work_page(
        &self,
        lang: &str,
        article_id: u64,
        page_number: u32,
    ) -> Result<(Vec<u64>, Option<u32>)> {
        if page_number == 0 {
            return Err(Error::Other("Pixivision pages start at one".into()));
        }
        if lang.is_empty()
            || !lang
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(Error::Other("Invalid pixivision language".to_string()));
        }
        let mut url =
            reqwest::Url::parse(&format!("https://www.pixivision.net/{lang}/a/{article_id}"))
                .map_err(|error| Error::Other(format!("Invalid pixivision URL: {error}")))?;
        if page_number > 1 {
            url.query_pairs_mut()
                .append_pair("p", &page_number.to_string());
        }
        let html = self.anonymous_get_text(url.clone()).await?;
        let page = crate::gallery::pixivision_page(&html, &url, lang, article_id, page_number)?;
        Ok((page.work_ids, page.next_page))
    }

    /// 获取 pixivision 正文中按展示顺序出现的全部作品 ID。
    pub async fn pixivision_work_ids(&self, lang: &str, article_id: u64) -> Result<Vec<u64>> {
        let mut page_number = 1;
        let mut work_ids = Vec::new();
        let mut seen = std::collections::HashSet::new();
        loop {
            let (ids, next_page) = self
                .pixivision_work_page(lang, article_id, page_number)
                .await?;
            work_ids.extend(ids.into_iter().filter(|work_id| seen.insert(*work_id)));
            match next_page {
                Some(next_page) => page_number = next_page,
                None => return Ok(work_ids),
            }
        }
    }

    /// 按精确标签查询指定日期范围，按作品发布时间从旧到新分页。
    pub async fn search_illusts(
        &self,
        word: &str,
        start_date: &str,
        end_date: &str,
        offset: u32,
    ) -> Result<SearchIllusts> {
        self.get(
            "/v1/search/illust",
            &[
                ("word", word.to_owned()),
                ("search_target", "exact_match_for_tags".to_owned()),
                ("sort", "date_asc".to_owned()),
                ("filter", "for_ios".to_owned()),
                ("start_date", start_date.to_owned()),
                ("end_date", end_date.to_owned()),
                ("offset", offset.to_string()),
            ],
        )
        .await
    }

    /// 按精确标签查询所有日期的作品，按最新发布优先分页。
    pub async fn search_tag_latest(&self, word: &str, offset: u32) -> Result<SearchIllusts> {
        self.get(
            "/v1/search/illust",
            &[
                ("word", word.to_owned()),
                ("search_target", "exact_match_for_tags".to_owned()),
                ("sort", "date_desc".to_owned()),
                ("filter", "for_ios".to_owned()),
                ("offset", offset.to_string()),
            ],
        )
        .await
    }

    /// 获取作品详情
    pub async fn illust_detail(&self, illust_id: u64) -> Result<IllustDetail> {
        let params = vec![("illust_id", illust_id.to_string())];
        self.get("/v1/illust/detail", &params).await
    }

    /// 获取排行榜
    ///
    /// # 参数
    /// - `mode`: 排行榜模式 (day, week, month, day_male, day_female, etc.)
    /// - `date`: 日期 (YYYY-MM-DD 格式，None 表示最新)
    /// - `offset`: 分页偏移量
    pub async fn illust_ranking(
        &self,
        mode: &str,
        date: Option<&str>,
        offset: Option<u32>,
    ) -> Result<Ranking> {
        let mut params = vec![
            ("mode", mode.to_string()),
            ("filter", "for_ios".to_string()),
        ];

        if let Some(d) = date {
            params.push(("date", d.to_string()));
        }

        if let Some(o) = offset {
            params.push(("offset", o.to_string()));
        }

        self.get("/v1/illust/ranking", &params).await
    }

    /// 获取用户详情
    ///
    /// # 参数
    /// - `user_id`: 用户 ID
    pub async fn user_detail(&self, user_id: u64) -> Result<UserDetail> {
        let params = vec![
            ("user_id", user_id.to_string()),
            ("filter", "for_ios".to_string()),
        ];
        self.get("/v1/user/detail", &params).await
    }

    /// 获取 Ugoira (动图) 元数据
    ///
    /// # 参数
    /// - `illust_id`: 作品 ID (必须是 ugoira 类型)
    ///
    /// # 返回
    /// 包含 ZIP 下载 URL 和每帧延迟信息
    pub async fn ugoira_metadata(&self, illust_id: u64) -> Result<UgoiraMetadata> {
        let params = vec![("illust_id", illust_id.to_string())];
        self.get("/v1/ugoira/metadata", &params).await
    }
}
